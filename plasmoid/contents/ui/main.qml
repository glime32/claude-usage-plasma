// Widget « Usage Claude » : session 5 h + semaine (anneaux), consommation du PC en watts,
// température du processeur (clic dessus = tableau de bord des capteurs), et une flèche
// qui replie / déplie ces quatre valeurs.
// Économie : aucune animation. Le panneau n'est redessiné que quand une valeur
// affichée change vraiment (hystérésis), jamais « à chaque image ». Replié : plus
// aucune lecture de watts ni de température (l'usage Claude continue, pour les notifs).
import QtQuick
import QtQuick.Layouts
import QtQuick.Shapes
import org.kde.plasma.plasmoid
import org.kde.plasma.components as PlasmaComponents
import org.kde.plasma.plasma5support as P5Support
import org.kde.kirigami as Kirigami

PlasmoidItem {
    id: root

    // ───────────── Repli (flèche) ─────────────
    readonly property bool collapsed: Plasmoid.configuration.collapsed
    function toggleCollapsed() {
        if (collapsed) refresh()      // déplier = actualiser aussi l'usage Claude
        else expanded = false
        Plasmoid.configuration.collapsed = !collapsed
    }
    onCollapsedChanged: if (!collapsed) { ema = -1; lastE = -1 }   // watts : repartir de la mesure fraîche

    // ───────────── Usage Claude ─────────────
    readonly property string cmd: '"$HOME/.local/bin/claude-usage"'
    readonly property int minPoll: 10 * 60000
    readonly property int maxPoll: 30 * 60000

    property int session: -1
    property int week: -1
    property real sessionReset: 0
    property real weekReset: 0
    property real updated: 0
    property bool stale: false
    property string error: ""
    property int failures: 0
    property real lastRun: 0

    function pct(v) { return v < 0 ? "– %" : v + " %" }
    function hour(t) { return new Date(t * 1000).toLocaleTimeString(Qt.locale(), "HH:mm") }
    function day(t) { return new Date(t * 1000).toLocaleString(Qt.locale(), "dddd d MMM 'à' HH:mm") }

    function refresh() {
        lastRun = Date.now()
        exec.connectSource(cmd)
    }

    function handle(out) {
        let d
        try { d = JSON.parse(out) } catch (e) { d = { ok: false, e: "script claude-usage introuvable" } }
        const changed = d.s !== undefined && (d.s !== session || d.w !== week)
        if (d.s !== undefined) {
            session = d.s; week = d.w
            sessionReset = d.sr; weekReset = d.wr
            updated = d.t
        }
        stale = !d.ok
        error = d.e || ""
        failures = d.ok ? 0 : failures + 1
        // Un seul nouvel essai rapide (ex. Wi-Fi pas encore prêt au réveil), mais jamais après
        // un refus « trop de requêtes » (d.rl) : là, on attend l'interrogation suivante.
        if (failures === 1 && !d.rl) retry.restart()

        // Interrogation adaptative : 10 min quand l'usage bouge (ou juste après un refus),
        // puis 20, puis 30 min.
        poll.interval = changed || (d.rl && failures === 1) ? minPoll : Math.min(poll.interval * 2, maxPoll)
        poll.restart()

        // Une seule minuterie, calée sur la prochaine réinitialisation : notif à l'heure pile.
        const now = Date.now() / 1000
        const next = [sessionReset, weekReset].filter(t => t > now)
        if (next.length) {
            resetTimer.interval = Math.min((Math.min(...next) - now + 20) * 1000, 2000000000)
            resetTimer.restart()
        } else {
            resetTimer.stop()
        }
    }

    // ───────────── Watts ─────────────
    // psys = compteur d'énergie de toute la plateforme (précis, marche aussi sur secteur,
    // mais lisible seulement si autorisé). Sinon : batterie (vraie conso totale quand on est sur batterie).
    readonly property string psysPath: "/sys/class/powercap/intel-rapl:1/energy_uj"
    readonly property string batPath: "/sys/class/power_supply/BAT0/uevent"
    readonly property string powerCmd: "cat " + batPath + " " + psysPath
    property bool xhrOk: true        // lecture directe dans plasmashell (QML_XHR_ALLOW_FILE_READ=1)
    property bool psysOk: true
    property real psysRetryAt: 0
    property real lastE: -1
    property real lastT: 0
    property real ema: -1
    property real watts: -1          // valeur affichée
    property string powerSource: ""  // "psys" | "battery" | "ac"

    function fmtW(w) {
        if (w < 0) return "– W"
        return (w < 10 ? w.toLocaleString(Qt.locale(), 'f', 1) : Math.round(w).toString()) + " W"
    }

    function push(w) {
        ema = ema < 0 ? w : ema + 0.5 * (w - ema)
        // Hystérésis : on ne change l'affichage (donc on ne redessine) que si l'écart se voit.
        if (watts < 0 || Math.abs(ema - watts) >= (ema < 10 ? 0.2 : 1)) watts = ema
    }

    function onPower(psysText, batText) {
        const now = Date.now()
        const e = psysText ? Number(psysText) : NaN
        if (!isNaN(e)) {
            powerSource = "psys"
            if (lastE >= 0 && e >= lastE && now - lastT < 10000) push((e - lastE) / ((now - lastT) * 1000))
            lastE = e; lastT = now
            return
        }
        lastE = -1
        const st = (batText.match(/POWER_SUPPLY_STATUS=(.*)/) || [])[1]
        const p = Number((batText.match(/POWER_SUPPLY_POWER_NOW=(\d+)/) || [])[1])
        if (st === "Discharging" && p > 0) {
            powerSource = "battery"
            push(p / 1e6)
        } else {
            powerSource = "ac"
            ema = -1
            watts = -1
        }
    }

    property int xhrPending: 0
    function readFile(path, cb) {
        xhrPending++
        const x = new XMLHttpRequest()
        x.onreadystatechange = () => {
            if (x.readyState === XMLHttpRequest.DONE) { xhrPending--; cb(x.status === 200 ? x.responseText : "") }
        }
        x.open("GET", "file://" + path)
        x.send()
    }

    function readBattery() {
        readFile(batPath, b => {
            if (!b) { xhrOk = false; return }  // lecture directe interdite : on passe au mode « cat »
            onPower("", b)
        })
    }

    function sample() {
        // La lecture d'il y a 1 s n'a jamais répondu : lecture directe interdite -> mode « cat ».
        if (xhrPending > 0) { xhrOk = false; return }
        if (psysOk || Date.now() >= psysRetryAt) {
            readFile(psysPath, t => {
                if (t) { psysOk = true; onPower(t, ""); return }
                psysOk = false
                psysRetryAt = Date.now() + 30000
                readBattery()
            })
        } else {
            readBattery()
        }
    }

    // ───────────── Températures ─────────────
    // Lues seulement si les valeurs sont dépliées, ou si le tableau de bord est ouvert.
    Temps {
        id: temps
        open: root.expanded
        active: !root.collapsed || root.expanded
    }

    // ───────────── Couleurs ─────────────
    readonly property color dim: Qt.rgba(Kirigami.Theme.textColor.r, Kirigami.Theme.textColor.g, Kirigami.Theme.textColor.b, 0.35)
    readonly property color track: Qt.rgba(Kirigami.Theme.textColor.r, Kirigami.Theme.textColor.g, Kirigami.Theme.textColor.b, 0.22)
    readonly property real secondary: 0.7       // opacité des textes secondaires du tableau
    // Anneaux : violet (ton thème #610387, éclairci pour rester visible en trait fin) -> jaune -> rouge.
    function level(v) {
        return v < 0 ? dim
             : v >= 85 ? "#ff3b4e"
             : v >= 50 ? "#ffcc1a"
             : "#9a2ee0"
    }
    readonly property color wattColor: watts < 0 ? dim
        : watts >= 18 ? Kirigami.Theme.negativeTextColor
        : watts >= 8 ? Kirigami.Theme.neutralTextColor
        : Kirigami.Theme.positiveTextColor
    function heat(v, warn, hot) {
        return v < 0 ? dim
             : v >= hot ? Kirigami.Theme.negativeTextColor
             : v >= warn ? Kirigami.Theme.neutralTextColor
             : Kirigami.Theme.positiveTextColor
    }
    // Dans le tableau, un chiffre normal reste en couleur de texte ; seul le chaud se colore.
    function textHeat(v, warn, hot) { return v < 0 ? dim : v >= warn ? heat(v, warn, hot) : Kirigami.Theme.textColor }

    readonly property string details:
        "Session (5 h) : " + pct(session) + (sessionReset ? ", réinitialisée à " + hour(sessionReset) : "")
        + "\nSemaine : " + pct(week) + (weekReset ? ", réinitialisée " + day(weekReset) : "")
        + "\n" + (error ? "Erreur : " + error : updated ? "Mis à jour à " + hour(updated) : "")
        + (collapsed ? ""
           : "\nWatts : " + (powerSource === "psys" ? "capteur psys (toute la machine)"
                            : powerSource === "battery" ? "batterie (conso totale)"
                            : "sur secteur, capteur psys non autorisé")
             + "\nProcesseur : " + temps.fmt(temps.shown)
             + (temps.peak >= 0 ? " (pic " + temps.fmt(temps.peak) + " à " + temps.hour(temps.peakAt) + ")" : ""))
        + "\nClic : actualiser l'usage Claude"
        + (collapsed ? "" : "\nClic sur la température : tableau de bord des capteurs")
        + "\nFlèche : " + (collapsed ? "afficher (et actualiser)" : "cacher") + " l'usage, les watts et la température"

    preferredRepresentation: compactRepresentation
    Plasmoid.icon: "chronometer"
    toolTipMainText: "Usage Claude"
    toolTipSubText: details
    toolTipTextFormat: Text.PlainText

    // Anneau de progression (dessiné une fois, redessiné seulement si la valeur change).
    component Ring: Item {
        id: g
        property int value: -1
        property string label
        property color color
        property color trackColor
        property color labelColor
        readonly property real stroke: Math.max(2, Math.round(width / 8))
        readonly property real r: (width - stroke) / 2

        Shape {
            anchors.fill: parent
            preferredRendererType: Shape.CurveRenderer
            ShapePath {
                strokeColor: g.trackColor
                strokeWidth: g.stroke
                fillColor: "transparent"
                PathAngleArc { centerX: g.width / 2; centerY: g.height / 2; radiusX: g.r; radiusY: g.r; startAngle: 0; sweepAngle: 360 }
            }
            ShapePath {
                strokeColor: g.value > 0 ? g.color : "transparent"
                strokeWidth: g.stroke
                fillColor: "transparent"
                capStyle: ShapePath.RoundCap
                PathAngleArc {
                    centerX: g.width / 2; centerY: g.height / 2; radiusX: g.r; radiusY: g.r
                    startAngle: -90
                    sweepAngle: 3.6 * Math.min(100, Math.max(0, g.value))
                }
            }
        }
        Text {
            anchors.centerIn: parent
            text: g.label
            color: g.labelColor
            font.pixelSize: Math.max(7, Math.round(g.width * 0.4))
            font.weight: Font.Bold
        }
    }

    component Bolt: Shape {
        id: b
        property color color
        preferredRendererType: Shape.CurveRenderer
        ShapePath {
            fillColor: b.color
            strokeColor: "transparent"
            scale: Qt.size(b.width / 10, b.height / 16)
            PathSvg { path: "M6.6 0 L0.6 9.2 L4.6 9.2 L3.2 16 L9.4 6.4 L5.4 6.4 L7.4 0 Z" }
        }
    }

    component Value: PlasmaComponents.Label {
        property int px
        font.pixelSize: px
        font.weight: Font.DemiBold
        font.features: ({ "tnum": 1 })
        Layout.alignment: Qt.AlignVCenter
    }

    compactRepresentation: MouseArea {
        id: compact
        readonly property int base: Math.min(height, Kirigami.Units.iconSizes.medium)
        readonly property int ring: Math.round(base * 0.78)              // anneaux un peu plus petits que les icônes
        readonly property int px: Math.max(10, Math.round(base * 0.6))   // texte des valeurs
        readonly property bool unfolded: !root.collapsed
        Layout.minimumWidth: row.implicitWidth  // pas de marge en plus : le panneau espace déjà les widgets
        Layout.preferredWidth: Layout.minimumWidth
        Layout.maximumWidth: Layout.minimumWidth
        Layout.fillHeight: true
        onClicked: root.refresh()

        // Les watts et la température changent souvent : largeur réservée au plus juste
        // (« 8,8 W » ou « 88 W », « 88 °C »), pour que la barre ne bouge pas à chaque mesure.
        TextMetrics {
            id: mW1
            font.family: Kirigami.Theme.defaultFont.family
            font.pixelSize: compact.px
            font.weight: Font.DemiBold
            font.features: ({ "tnum": 1 })
            text: "8,8 W"
        }
        TextMetrics { id: mW2; font: mW1.font; text: "88 W" }
        TextMetrics { id: mT; font: mW1.font; text: "88 °C" }

        RowLayout {
            id: row
            anchors.centerIn: parent
            // Toute la hauteur de la barre : sinon, replié, la flèche (seul élément visible,
            // sans hauteur propre) aurait une zone de clic de 0 px.
            height: parent.height
            spacing: Kirigami.Units.largeSpacing + Kirigami.Units.smallSpacing  // même écart qu'entre les icônes de la barre

            RowLayout {
                visible: compact.unfolded
                spacing: Kirigami.Units.smallSpacing
                opacity: root.stale ? 0.55 : 1
                Ring {
                    Layout.preferredWidth: compact.ring; Layout.preferredHeight: compact.ring
                    value: root.session; label: "5h"
                    color: root.level(root.session); trackColor: root.track; labelColor: Kirigami.Theme.textColor
                }
                Value { px: compact.px; text: root.pct(root.session) }
            }

            RowLayout {
                visible: compact.unfolded
                spacing: Kirigami.Units.smallSpacing
                opacity: root.stale ? 0.55 : 1
                Ring {
                    Layout.preferredWidth: compact.ring; Layout.preferredHeight: compact.ring
                    value: root.week; label: "7j"
                    color: root.level(root.week); trackColor: root.track; labelColor: Kirigami.Theme.textColor
                }
                Value { px: compact.px; text: root.pct(root.week) }
            }

            RowLayout {
                visible: compact.unfolded
                spacing: Math.round(Kirigami.Units.smallSpacing / 2)
                Bolt {
                    Layout.preferredWidth: Math.round(compact.ring * 0.55)
                    Layout.preferredHeight: Math.round(compact.ring * 0.85)
                    color: root.wattColor
                }
                Value {
                    px: compact.px; text: root.fmtW(root.watts)
                    Layout.preferredWidth: Math.ceil(Math.max(mW1.advanceWidth, mW2.advanceWidth))
                    color: root.watts < 0 ? root.dim : Kirigami.Theme.textColor
                }
            }

            // Température : son clic ouvre le tableau de bord (pas l'actualisation Claude).
            MouseArea {
                visible: compact.unfolded
                Layout.fillHeight: true
                Layout.preferredWidth: tRow.implicitWidth
                onClicked: root.expanded = !root.expanded
                RowLayout {
                    id: tRow
                    anchors.centerIn: parent
                    spacing: Math.round(Kirigami.Units.smallSpacing / 2)
                    Thermo {
                        Layout.preferredWidth: Math.round(compact.ring * 0.5)
                        Layout.preferredHeight: Math.round(compact.ring * 0.9)
                        value: temps.shown
                        color: root.heat(temps.shown, 70, 90)
                        trackColor: root.track
                    }
                    Value {
                        px: compact.px
                        text: temps.fmt(temps.shown)
                        Layout.preferredWidth: Math.max(Math.ceil(mT.advanceWidth), implicitWidth)
                        color: temps.shown < 0 ? root.dim : Kirigami.Theme.textColor
                    }
                }
            }

            // Flèche au bord droit : elle ne bouge pas quand les valeurs se replient,
            // donc un 2e clic au même endroit les ré-affiche. ‹ = afficher, › = cacher.
            MouseArea {
                Layout.fillHeight: true
                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                onClicked: root.toggleCollapsed()
                Kirigami.Icon {
                    anchors.centerIn: parent
                    width: Kirigami.Units.iconSizes.small
                    height: width
                    source: root.collapsed ? "arrow-left" : "arrow-right"
                }
            }
        }
    }

    // ───────────── Tableau de bord des températures ─────────────
    component Num: PlasmaComponents.Label {
        font.weight: Font.DemiBold
        font.features: ({ "tnum": 1 })
        horizontalAlignment: Text.AlignRight
    }

    component Reading: RowLayout {
        id: rd
        property var sensor
        readonly property real v: temps.values[sensor.path] !== undefined ? temps.values[sensor.path] : -1
        spacing: Kirigami.Units.smallSpacing
        Layout.fillWidth: true
        Layout.preferredWidth: 1                 // colonnes de même largeur
        PlasmaComponents.Label { text: rd.sensor.label; Layout.fillWidth: true; elide: Text.ElideRight }
        Num { text: temps.fmt(rd.v); color: root.textHeat(rd.v, rd.sensor.warn, rd.sensor.hot) }
    }

    component Section: ColumnLayout {
        id: sec
        property string title
        property var items: []
        property int columns: 2
        property string note
        visible: sec.items.length > 0
        spacing: Kirigami.Units.smallSpacing
        Layout.fillWidth: true
        Kirigami.Heading { level: 5; text: sec.title }
        GridLayout {
            columns: sec.columns
            columnSpacing: Kirigami.Units.gridUnit
            rowSpacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true
            Repeater { model: sec.items; Reading { required property var modelData; sensor: modelData } }
        }
        PlasmaComponents.Label {
            visible: sec.note !== ""
            text: sec.note
            opacity: root.secondary
            font: Kirigami.Theme.smallFont
            wrapMode: Text.WordWrap
            Layout.fillWidth: true
        }
    }

    fullRepresentation: Item {
        readonly property int m: Kirigami.Units.largeSpacing
        Layout.minimumWidth: Kirigami.Units.gridUnit * 20
        Layout.preferredWidth: Layout.minimumWidth
        Layout.minimumHeight: col.implicitHeight + 2 * m
        Layout.preferredHeight: Layout.minimumHeight
        Layout.maximumHeight: Layout.minimumHeight

        ColumnLayout {
            id: col
            anchors.fill: parent
            anchors.margins: parent.m
            spacing: Kirigami.Units.largeSpacing

            // Processeur : grand chiffre + pic
            RowLayout {
                spacing: Kirigami.Units.largeSpacing
                Layout.fillWidth: true
                Thermo {
                    Layout.preferredWidth: Math.round(Kirigami.Units.gridUnit * 1.1)
                    Layout.preferredHeight: Kirigami.Units.gridUnit * 2
                    value: temps.cpu
                    color: root.heat(temps.cpu, 70, 90)
                    trackColor: root.track
                }
                Num {
                    text: temps.fmt(temps.cpu)
                    font.pixelSize: Kirigami.Units.gridUnit * 2
                    color: root.textHeat(temps.cpu, 70, 90)
                }
                ColumnLayout {
                    spacing: 0
                    Layout.fillWidth: true
                    Kirigami.Heading { level: 4; text: "Processeur" }
                    PlasmaComponents.Label {
                        text: temps.peak >= 0 ? "Pic : " + temps.fmt(temps.peak) + " à " + temps.hour(temps.peakAt) : ""
                        opacity: root.secondary
                    }
                }
            }

            // Courbe des 2 dernières minutes (calculée seulement tableau ouvert)
            ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true
                Item {
                    id: spark
                    Layout.fillWidth: true
                    Layout.preferredHeight: Kirigami.Units.gridUnit * 2.5
                    readonly property var h: root.expanded ? temps.history : []
                    readonly property real lo: h.length ? Math.min(...h) : 0
                    readonly property real hi: h.length ? Math.max(...h) : 0
                    readonly property real span: Math.max(10, hi - lo)
                    readonly property real base: (hi + lo) / 2 - span / 2
                    readonly property real step: width / (temps.historyLen - 1)
                    readonly property var pts: {
                        const p = []
                        for (let i = 0; i < h.length; i++)
                            p.push(Qt.point(width - (h.length - 1 - i) * step,
                                            2 + (1 - (h[i] - base) / span) * (height - 4)))
                        return p
                    }
                    Shape {
                        anchors.fill: parent
                        visible: spark.pts.length > 1
                        preferredRendererType: Shape.CurveRenderer
                        ShapePath {
                            strokeColor: "transparent"
                            fillColor: Qt.rgba(Kirigami.Theme.highlightColor.r, Kirigami.Theme.highlightColor.g, Kirigami.Theme.highlightColor.b, 0.18)
                            PathPolyline {
                                path: spark.pts.length > 1
                                    ? [Qt.point(spark.pts[0].x, spark.height)].concat(spark.pts, [Qt.point(spark.width, spark.height)])
                                    : []
                            }
                        }
                        ShapePath {
                            strokeColor: Kirigami.Theme.highlightColor
                            strokeWidth: 2
                            fillColor: "transparent"
                            joinStyle: ShapePath.RoundJoin
                            capStyle: ShapePath.RoundCap
                            PathPolyline { path: spark.pts }
                        }
                    }
                    Rectangle {
                        anchors { left: parent.left; right: parent.right; bottom: parent.bottom }
                        height: 1
                        color: root.track
                    }
                }
                PlasmaComponents.Label {
                    text: spark.h.length > 1
                        ? "2 dernières minutes : de " + Math.round(spark.lo) + " à " + temps.fmt(spark.hi)
                        : "Courbe des 2 dernières minutes : en cours…"
                    opacity: root.secondary
                    font: Kirigami.Theme.smallFont
                }
            }

            Section {
                title: "Cœurs (P = performants, E = économes)"
                items: temps.group("core")
                columns: 4
            }
            Section { title: "Carte mère"; items: temps.group("board") }
            Section {
                title: "Stockage et réseau"
                items: temps.group("disk").concat(temps.group("other"))
                note: temps.diskPaths.length ? "Le SSD est lu toutes les 10 s seulement : chaque lecture le réveille." : ""
            }

            PlasmaComponents.Label {
                text: temps.updated ? "Actualisé à " + temps.hour(temps.updated, true) + " · toutes les 2 s" : ""
                opacity: root.secondary
                font: Kirigami.Theme.smallFont
                Layout.alignment: Qt.AlignRight
            }
        }
    }

    P5Support.DataSource {
        id: exec
        engine: "executable"
        connectedSources: []
        onNewData: (source, data) => {
            disconnectSource(source)
            root.handle(data["stdout"])
        }
    }

    // Mode de secours pour les watts si la lecture directe est interdite :
    // un simple « cat » (sans shell) toutes les 2 s, soit ~1,5 ms de CPU. Rien si replié.
    P5Support.DataSource {
        engine: "executable"
        interval: 2000
        connectedSources: root.xhrOk || root.collapsed ? [] : [root.powerCmd]
        onNewData: (source, data) => {
            const out = (data["stdout"] || "").trim()
            const last = out.slice(out.lastIndexOf("\n") + 1)
            root.onPower(/^\d+$/.test(last) ? last : "", out)
        }
    }

    // Après une mise en veille, le moteur « time » signale le saut d'horloge :
    // on actualise tout de suite. Fuseau que rien d'autre n'utilise + intervalle 0
    // = jamais de réveil périodique.
    P5Support.DataSource {
        engine: "time"
        connectedSources: ["Pacific/Kiritimati"]
        interval: 0
        onNewData: if (Date.now() - root.lastRun > 120000) root.refresh()
    }

    Timer { id: poll; interval: root.minPoll; repeat: true; running: true; onTriggered: root.refresh() }
    Timer { id: resetTimer; repeat: false; onTriggered: root.refresh() }
    // Nouvel essai après 150 s : le script refuse de rappeler l'API moins de 2 min après.
    Timer { id: retry; interval: 150000; repeat: false; onTriggered: root.refresh() }
    // Watts : 1 lecture/s en lecture directe (quasi gratuite), rien si replié ;
    // sinon c'est le mode « cat » ci-dessus.
    Timer { interval: 1000; repeat: true; running: root.xhrOk && !root.collapsed; triggeredOnStart: true; onTriggered: root.sample() }

    Component.onCompleted: refresh()
}
