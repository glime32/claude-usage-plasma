// Lecture des températures (aucun affichage ici).
// Économie : une lecture toutes les 2 s, d'UN seul fichier tant que le tableau de bord est
// fermé, et aucune quand le widget est replié. La valeur de la barre a une hystérésis.
// Le SSD n'est lu que tableau ouvert, toutes les 10 s : chaque lecture le sort de sa
// veille et coûte ~10 ms.
import QtQuick
import org.kde.plasma.plasma5support as P5Support

Item {
    id: t

    property bool open: false       // tableau de bord ouvert : tous les capteurs
    property bool active: true      // faux = widget replié : plus aucune lecture

    readonly property int period: 2000
    readonly property int diskEvery: 5          // SSD : une lecture sur 5 (10 s)
    readonly property int historyLen: 60        // courbe : 60 mesures = 2 min

    // ───────────── Inventaire (sensors.sh, une seule fois) ─────────────
    readonly property string script: decodeURIComponent(Qt.resolvedUrl("../code/sensors.sh").toString().replace(/^file:\/\//, ""))
    readonly property string inventoryCmd: "sh " + script
    property var sensors: []                    // [{ g, label, path, warn, hot }]
    property string cpuPath: ""
    property bool ready: false
    readonly property var fastPaths: sensors.filter(s => s.g !== "disk").map(s => s.path)
    readonly property var diskPaths: sensors.filter(s => s.g === "disk").map(s => s.path)
    function group(g) { return sensors.filter(s => s.g === g) }

    function onInventory(out) {
        const list = []
        for (const line of (out || "").split("\n")) {
            const f = line.split("|")
            if (f.length === 5) list.push({ g: f[0], label: f[1], path: f[2], warn: Number(f[3]), hot: Number(f[4]) })
        }
        const c = list.find(s => s.g === "cpu") || list[0]
        sensors = list
        cpuPath = c ? c.path : ""
        ready = cpuPath !== ""
        if (!ready) { inventoryRetry.start(); return }
        if (active) tick(true)
        sync()
    }

    // ───────────── Mesures ─────────────
    property real cpu: -1                       // dernière mesure du processeur (°C)
    property int shown: -1                      // valeur affichée dans la barre
    property int candidate: -1                  // changement d'1 °C en attente de confirmation
    property real peak: -1
    property real peakAt: 0
    property var history: []
    property var values: ({})                   // fichier -> °C, pour le tableau
    property real updated: 0
    property int ticks: 0

    function fmt(v) { return v < 0 || isNaN(v) ? "– °C" : Math.round(v) + " °C" }
    function hour(ms, sec) { return new Date(ms).toLocaleTimeString(Qt.locale(), sec ? "HH:mm:ss" : "HH:mm") }

    function apply(out) {
        const now = Date.now()
        const c = out[cpuPath]
        if (c !== undefined && !isNaN(c)) {
            cpu = c
            // Hystérésis : un écart de 2 °C s'affiche tout de suite, un écart de 1 °C
            // seulement s'il est confirmé par la mesure suivante (pas de clignotement 52/53).
            // Tableau ouvert : valeur exacte, pour que la barre et le tableau disent la même chose.
            const r = Math.round(c)
            if (open || shown < 0 || Math.abs(r - shown) >= 2 || r === candidate) shown = r
            candidate = r !== shown ? r : -1
            const h = history.slice(1 - historyLen)
            h.push(c)
            history = h
            if (c > peak) { peak = c; peakAt = now }
        }
        if (open) values = Object.assign({}, values, out)
        updated = now
    }

    // Une lecture : le processeur seul quand le tableau est fermé, tout sinon.
    function tick(withDisk) {
        if (!ready) return
        ticks++
        const paths = !open ? [cpuPath]
                    : withDisk || ticks % diskEvery === 0 ? fastPaths.concat(diskPaths)
                    : fastPaths
        if (xhrOk) readDirect(paths)
        else exec.connectSource(paths.length === 1 ? "cat " + paths[0] : "grep -sH . " + paths.join(" "))
    }

    function sync() {
        if (ready && active) poll.restart()
        else poll.stop()
    }
    onOpenChanged: if (active) { tick(open); sync() }       // clic = mesure immédiate
    onActiveChanged: { if (active) tick(open); sync() }      // déplié = mesure immédiate

    // Lecture directe dans plasmashell (gratuite) si QML_XHR_ALLOW_FILE_READ=1,
    // sinon une commande « cat »/« grep » par lecture (~1 ms de CPU).
    property bool xhrOk: true
    property int xhrPending: 0
    function readDirect(paths) {
        if (xhrPending > 0) { fallback(); return }   // la lecture précédente n'a jamais répondu
        const out = {}
        let left = paths.length
        try {
            for (const p of paths) {
                const x = new XMLHttpRequest()
                xhrPending++
                x.onreadystatechange = () => {
                    if (x.readyState !== XMLHttpRequest.DONE) return
                    xhrPending--
                    const txt = (x.responseText || "").trim()
                    if (txt !== "") out[p] = Number(txt) / 1000
                    if (--left > 0) return
                    if (out[cpuPath] === undefined) fallback()
                    else apply(out)
                }
                x.open("GET", "file://" + p)
                x.send()
            }
        } catch (e) {
            fallback()
        }
        probe.restart()
    }
    function fallback() {
        if (!xhrOk) return
        xhrOk = false
        tick(true)
    }

    function onRead(source, stdout) {
        const out = {}
        const text = stdout || ""
        if (source.startsWith("cat ")) {
            if (text.trim() !== "") out[cpuPath] = Number(text) / 1000
        } else {
            for (const line of text.split("\n")) {
                const i = line.lastIndexOf(":")
                if (i > 0) out[line.slice(0, i)] = Number(line.slice(i + 1)) / 1000
            }
        }
        apply(out)
    }

    P5Support.DataSource {
        id: exec
        engine: "executable"
        connectedSources: []
        onNewData: (source, data) => {
            disconnectSource(source)
            if (source === t.inventoryCmd) t.onInventory(data["stdout"])
            else t.onRead(source, data["stdout"])
        }
    }

    Timer { id: poll; interval: t.period; repeat: true; onTriggered: t.tick(false) }
    // Lecture directe interdite : la 1re requête ne répond jamais -> mode commande sans attendre 2 s.
    Timer { id: probe; interval: 300; onTriggered: if (t.xhrPending > 0) t.fallback() }
    Timer { id: inventoryRetry; interval: 30000; onTriggered: exec.connectSource(t.inventoryCmd) }

    Component.onCompleted: exec.connectSource(inventoryCmd)
}
