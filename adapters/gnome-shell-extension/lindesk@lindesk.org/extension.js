// lindesk GNOME Shell adapter.
//
// Reports to lindesk-session (session bus name org.lindesk.QoS1):
//   * focus changes      -> SetForegroundProcess(pid)
//   * user input         -> BeginInteraction(pid of the focused window), at most every 250 ms
//   * frame overruns     -> ReportMetric(...) every 5 s (off by default, see FRAME_METRICS)
//
// Every call is asynchronous and fire-and-forget. Errors (most commonly "lindesk is not
// running") are swallowed: this extension must never slow down or break the shell.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'org.lindesk.QoS1';
const OBJECT_PATH = '/org/lindesk/QoS1';
const INTERFACE_NAME = 'org.lindesk.QoS1';

// Only the members this extension calls. Must match docs/ARCHITECTURE.md.
const INTERFACE_XML = `
<node>
  <interface name="org.lindesk.QoS1">
    <method name="SetForegroundProcess">
      <arg type="i" name="pid" direction="in"/>
    </method>
    <method name="BeginInteraction">
      <arg type="i" name="pid" direction="in"/>
    </method>
    <method name="ReportMetric">
      <arg type="s" name="name" direction="in"/>
      <arg type="d" name="value" direction="in"/>
      <arg type="a{ss}" name="labels" direction="in"/>
    </method>
  </interface>
</node>`;

// Minimum spacing between two BeginInteraction calls, in microseconds.
const INTERACTION_THROTTLE_US = 250 * 1000;

// While the user is active, how often the idle monitor is sampled (ms). Input that reaches
// Wayland clients bypasses the stage's captured-event signal, so the idle monitor is what
// catches typing/clicking inside applications.
const ACTIVE_POLL_MS = 250;

// Frame overrun metrics: off by default. Flip to true (or wire it to a GSettings key) to
// report gnome.frame_overruns / gnome.frame_max_ms every FRAME_REPORT_SECONDS.
const FRAME_METRICS = false;
const FRAME_REPORT_SECONDS = 5;
// A frame interval above this multiple of the refresh interval counts as an overrun.
const OVERRUN_FACTOR = 1.5;
// Intervals longer than this many refresh intervals are idle gaps (nothing was animating),
// not dropped frames, and are ignored.
const IDLE_GAP_FACTOR = 8;

const INPUT_EVENT_TYPES = new Set([
    Clutter.EventType.KEY_PRESS,
    Clutter.EventType.BUTTON_PRESS,
    Clutter.EventType.TOUCH_BEGIN,
    Clutter.EventType.SCROLL,
]);

function focusPid() {
    const win = global.display.focus_window;
    if (!win)
        return 0;
    const pid = win.get_pid();
    return pid > 0 ? pid : 0;
}

export default class LindeskExtension extends Extension {
    enable() {
        this._cancellable = new Gio.Cancellable();
        this._proxy = null;
        this._lastInteractionUs = 0;
        this._lastFocusPid = 0;
        this._signals = [];
        this._activeWatchId = 0;
        this._activePollId = 0;
        this._idleMonitor = null;
        this._frames = new Map();
        this._viewLabels = new WeakMap();
        this._nextViewIndex = 0;
        this._frameReportId = 0;

        const info = Gio.DBusNodeInfo.new_for_xml(INTERFACE_XML).lookup_interface(INTERFACE_NAME);
        Gio.DBusProxy.new(
            Gio.DBus.session,
            Gio.DBusProxyFlags.DO_NOT_AUTO_START |
                Gio.DBusProxyFlags.DO_NOT_LOAD_PROPERTIES |
                Gio.DBusProxyFlags.DO_NOT_CONNECT_SIGNALS,
            info, BUS_NAME, OBJECT_PATH, INTERFACE_NAME, this._cancellable,
            (_source, res) => {
                try {
                    this._proxy = Gio.DBusProxy.new_finish(res);
                } catch {
                    return; // cancelled (disabled before ready) or no session bus
                }
                // When lindesk-session (re)starts, tell it what is focused right now.
                this._ownerId = this._proxy.connect('notify::g-name-owner', () => {
                    if (this._proxy?.g_name_owner)
                        this._sendFocus(true);
                });
                this._sendFocus(true);
            });

        this._connect(global.display, 'notify::focus-window', () => this._sendFocus(false));
        this._connect(global.stage, 'captured-event', (_actor, event) => {
            if (INPUT_EVENT_TYPES.has(event.type()))
                this._noteInput();
            return Clutter.EVENT_PROPAGATE;
        });

        try {
            this._idleMonitor = global.backend.get_core_idle_monitor();
            this._armActiveWatch();
        } catch (e) {
            this._idleMonitor = null;
            console.debug(`lindesk: idle monitor unavailable: ${e}`);
        }

        if (FRAME_METRICS) {
            this._connect(global.stage, 'after-paint', (_stage, view) => this._onAfterPaint(view));
            this._frameReportId = GLib.timeout_add_seconds(GLib.PRIORITY_LOW, FRAME_REPORT_SECONDS, () => {
                this._reportFrames();
                return GLib.SOURCE_CONTINUE;
            });
        }
    }

    disable() {
        this._cancellable?.cancel();
        this._cancellable = null;

        for (const [obj, id] of this._signals)
            obj.disconnect(id);
        this._signals = [];

        if (this._activeWatchId && this._idleMonitor)
            this._idleMonitor.remove_watch(this._activeWatchId);
        this._activeWatchId = 0;
        this._idleMonitor = null;
        if (this._activePollId)
            GLib.source_remove(this._activePollId);
        this._activePollId = 0;

        if (this._frameReportId)
            GLib.source_remove(this._frameReportId);
        this._frameReportId = 0;
        this._frames = null;
        this._viewLabels = null;

        if (this._proxy && this._ownerId)
            this._proxy.disconnect(this._ownerId);
        this._ownerId = 0;
        this._proxy = null;
    }

    _connect(obj, signal, handler) {
        this._signals.push([obj, obj.connect(signal, handler)]);
    }

    _call(method, params) {
        if (!this._proxy)
            return;
        this._proxy.call(method, params, Gio.DBusCallFlags.NO_AUTO_START, -1, null, (proxy, res) => {
            try {
                proxy.call_finish(res);
            } catch {
                // lindesk not running, or an older lindesk: ignore.
            }
        });
    }

    _sendFocus(force) {
        const pid = focusPid();
        if (pid === 0 || (!force && pid === this._lastFocusPid))
            return;
        this._lastFocusPid = pid;
        this._call('SetForegroundProcess', new GLib.Variant('(i)', [pid]));
    }

    // Hot path: called for every key/button/touch/scroll. Keep it to a clock read and a compare.
    _noteInput() {
        const now = GLib.get_monotonic_time();
        if (now - this._lastInteractionUs < INTERACTION_THROTTLE_US)
            return;
        this._lastInteractionUs = now;
        const pid = focusPid();
        if (pid !== 0)
            this._call('BeginInteraction', new GLib.Variant('(i)', [pid]));
    }

    // Idle monitor path. While idle, a one-shot "user active" watch waits for the next input.
    // While active, the idle time is sampled every ACTIVE_POLL_MS; once the user has been idle
    // longer than that the poll stops and the watch is re-armed, so nothing runs while idle.
    _armActiveWatch() {
        this._activeWatchId = this._idleMonitor.add_user_active_watch(() => {
            this._activeWatchId = 0;
            this._noteInput();
            this._activePollId = GLib.timeout_add(GLib.PRIORITY_LOW, ACTIVE_POLL_MS, () => {
                if (this._idleMonitor && this._idleMonitor.get_idletime() < ACTIVE_POLL_MS) {
                    this._noteInput();
                    return GLib.SOURCE_CONTINUE;
                }
                this._activePollId = 0;
                if (this._idleMonitor)
                    this._armActiveWatch();
                return GLib.SOURCE_REMOVE;
            });
        });
    }

    _viewLabel(view) {
        let label = this._viewLabels.get(view);
        if (label === undefined) {
            label = `view${this._nextViewIndex++}`;
            try {
                const r = view.get_layout();
                if (r)
                    label = `${r.width}x${r.height}+${r.x}+${r.y}`;
            } catch {
                // keep the index label
            }
            this._viewLabels.set(view, label);
        }
        return label;
    }

    _onAfterPaint(view) {
        if (!view || !this._frames)
            return;
        const now = GLib.get_monotonic_time();
        const label = this._viewLabel(view);
        let s = this._frames.get(label);
        if (!s) {
            let hz = 60;
            try {
                hz = view.get_refresh_rate() || 60;
            } catch {
                // keep 60
            }
            s = {last: 0, refreshUs: 1e6 / hz, overruns: 0, maxUs: 0, frames: 0};
            this._frames.set(label, s);
        }
        if (s.last !== 0) {
            const dt = now - s.last;
            if (dt < s.refreshUs * IDLE_GAP_FACTOR) {
                s.frames++;
                if (dt > s.maxUs)
                    s.maxUs = dt;
                if (dt > s.refreshUs * OVERRUN_FACTOR)
                    s.overruns++;
            }
        }
        s.last = now;
    }

    _reportFrames() {
        if (!this._frames)
            return;
        for (const [monitor, s] of this._frames) {
            if (s.frames === 0)
                continue;
            const labels = {monitor};
            this._call('ReportMetric', new GLib.Variant('(sda{ss})', ['gnome.frame_overruns', s.overruns, labels]));
            this._call('ReportMetric', new GLib.Variant('(sda{ss})', ['gnome.frame_max_ms', s.maxUs / 1000, labels]));
            s.overruns = 0;
            s.maxUs = 0;
            s.frames = 0;
        }
    }
}
