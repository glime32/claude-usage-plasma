#!/bin/sh
# Inventaire des capteurs de température. Lancé UNE seule fois au démarrage du widget :
# les numéros hwmonN / thermal_zoneN peuvent changer d'un démarrage à l'autre.
# Sortie, une ligne par capteur :  groupe|libellé|fichier|tiède|chaud   (seuils en °C)
#   groupe : cpu (température principale), core (cœurs), board (carte mère), disk (lecture lente)
#            ou other (Wi-Fi et tout capteur inconnu).
export LC_ALL=C

p_list=$(cat /sys/devices/cpu_core/cpus 2>/dev/null)   # cœurs performants, ex. « 0-3 »

in_list() {  # in_list 5 "0-3,8" : vrai si 5 est dans la liste
    for r in $(echo "$2" | tr ',' ' '); do
        [ "$1" -ge "${r%-*}" ] && [ "$1" -le "${r#*-}" ] && return 0
    done
    return 1
}

core_kind() {  # core_kind <core_id> : P (performant) ou E (économe)
    for c in /sys/devices/system/cpu/cpu[0-9]*; do
        [ "$(cat "$c/topology/core_id" 2>/dev/null)" = "$1" ] || continue
        in_list "${c##*cpu}" "$p_list" && echo P || echo E
        return
    done
    echo C
}

hw_names=" "
cores=""
for h in /sys/class/hwmon/hwmon*; do
    name=$(cat "$h/name" 2>/dev/null)
    hw_names="$hw_names$name "
    for t in "$h"/temp*_input; do
        [ -r "$t" ] || continue
        label=$(cat "${t%_input}_label" 2>/dev/null)
        case "$name" in
        coretemp)
            case "$label" in
            Package*) echo "cpu|Processeur|$t|70|90" ;;
            Core\ *)  id=${label#Core }
                      cores="$cores$(core_kind "$id") $id $t
" ;;
            esac ;;
        nvme)
            case "$label" in
            Composite) echo "disk|SSD|$t|65|78" ;;
            *)         echo "disk|SSD ${label#Sensor }|$t|65|78" | sed 's/SSD \([0-9]\)/SSD capteur \1/' ;;
            esac ;;
        acpitz)    acpi_line="board|Sonde ACPI|$t|65|80" ;;   # affichée après les capteurs nommés
        iwlwifi*)  echo "other|Wi-Fi|$t|75|90" ;;
        *)         echo "other|$name${label:+ $label}|$t|65|85" ;;
        esac
    done
done

# Cœurs triés : P1, P2… puis E1, E2…
printf '%s' "$cores" | sort -k1,1r -k2,2n | awk 'NF { n[$1]++; print "core|" $1 n[$1] "|" $3 "|70|90" }'

# Zones thermiques du firmware (capteurs de la carte mère), nommées par leur description ACPI.
acpi_paths=$(grep -sH . /sys/bus/acpi/devices/*/path)
for z in /sys/class/thermal/thermal_zone*; do
    type=$(cat "$z/type" 2>/dev/null)
    case "$hw_names" in *" $type "*) continue ;; esac     # déjà lu via hwmon (acpitz, Wi-Fi)
    case "$type" in
    x86_pkg_temp|TCPU*|INT3400*) continue ;;             # doublons du processeur / valeur factice
    esac
    dev=$(printf '%s\n' "$acpi_paths" | sed -n "s|/path:.*\.$type\$||p" | head -n1)
    desc=$(cat "$dev/description" 2>/dev/null)
    case "$desc" in
    "CPU temperature")     label="Près du processeur" ;;
    "PMIC temperature")    label="Alimentation (PMIC)" ;;
    "Charger temperature") label="Chargeur" ;;
    "Ambient temperature") label="Intérieur du boîtier" ;;
    "Type temperature")    label="Zone USB-C" ;;
    "")                    label="$type" ;;
    *)                     label="$desc" ;;
    esac
    echo "board|$label|$z/temp|65|80"
done
[ -n "$acpi_line" ] && echo "$acpi_line"
exit 0
