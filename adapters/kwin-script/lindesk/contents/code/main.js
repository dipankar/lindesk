// lindesk KWin script: report the focused window's pid to lindesk-session.
//
// Calls org.lindesk.QoS1.SetForegroundProcess(i) on the session bus whenever the active
// window changes. callDBus is asynchronous; if lindesk is not running the call fails
// silently and KWin is unaffected.
//
// KWin scripts cannot observe input events, so on KDE lindesk's interaction epochs come from
// focus changes only (see README.md).

var SERVICE = "org.lindesk.QoS1";
var PATH = "/org/lindesk/QoS1";
var IFACE = "org.lindesk.QoS1";

var lastPid = 0;

function report(win) {
    if (!win) {
        // Focus went to nothing (desktop, or between windows): report the next window
        // even if it belongs to the same process as before.
        lastPid = 0;
        return;
    }
    var pid = win.pid;
    if (!(pid > 0) || pid === lastPid) {
        return;
    }
    lastPid = pid;
    callDBus(SERVICE, PATH, IFACE, "SetForegroundProcess", pid);
}

if (workspace.windowActivated !== undefined) {
    // KWin 6 (Plasma 6): workspace.activeWindow / windowActivated(Window).
    workspace.windowActivated.connect(report);
    report(workspace.activeWindow);
} else {
    // KWin 5 (Plasma 5): workspace.activeClient / clientActivated(Client).
    workspace.clientActivated.connect(report);
    report(workspace.activeClient);
}
