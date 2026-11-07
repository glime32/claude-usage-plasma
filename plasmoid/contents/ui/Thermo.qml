// Thermomètre : contour (piste) + mercure dont la hauteur suit la température (30 → 100 °C).
// Dans un Item : la taille d'un Shape suit son tracé, qui suit ici la taille (boucle sinon).
import QtQuick
import QtQuick.Shapes

Item {
    id: th
    property real value: -1
    property color color
    property color trackColor
    readonly property real s: Math.max(1, Math.round(width / 10))     // épaisseur du contour
    readonly property real cx: width / 2
    readonly property real a: width * 0.25                            // demi-largeur du tube
    readonly property real r: width / 2 - s / 2                       // rayon du réservoir
    readonly property real cy: height - s / 2 - r
    readonly property real y0: s / 2 + a
    readonly property real yj: cy - Math.sqrt(r * r - a * a)
    readonly property real ai: Math.max(0.75, a - s * 1.4)           // mercure, tube
    readonly property real ri: r - s * 1.4                            // mercure, réservoir
    readonly property real fill: value < 0 ? 0 : Math.min(1, Math.max(0.1, (value - 30) / 70))
    readonly property real yt: cy - fill * (cy - y0)

    Shape {
        anchors.fill: parent
        preferredRendererType: Shape.CurveRenderer
        ShapePath {
            strokeColor: th.trackColor
            strokeWidth: th.s
            fillColor: "transparent"
            PathSvg {
                path: `M ${th.cx - th.a} ${th.yj} L ${th.cx - th.a} ${th.y0}`
                    + ` A ${th.a} ${th.a} 0 0 1 ${th.cx + th.a} ${th.y0} L ${th.cx + th.a} ${th.yj}`
                    + ` A ${th.r} ${th.r} 0 1 1 ${th.cx - th.a} ${th.yj} Z`
            }
        }
        ShapePath {
            strokeColor: "transparent"
            fillColor: th.value < 0 ? "transparent" : th.color
            fillRule: ShapePath.WindingFill
            PathSvg {
                path: `M ${th.cx - th.ri} ${th.cy} A ${th.ri} ${th.ri} 0 1 1 ${th.cx + th.ri} ${th.cy}`
                    + ` A ${th.ri} ${th.ri} 0 1 1 ${th.cx - th.ri} ${th.cy} Z`
                    + ` M ${th.cx - th.ai} ${th.cy} L ${th.cx - th.ai} ${th.yt} L ${th.cx + th.ai} ${th.yt}`
                    + ` L ${th.cx + th.ai} ${th.cy} Z`
            }
        }
    }
}
