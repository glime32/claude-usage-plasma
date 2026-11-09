#!/bin/sh
# Installe (ou met à jour) le widget « Usage Claude » et son assistant claude-usage (Rust).
#   ./install.sh            installe
#   ./install.sh --test     + envoie une notification de test (PC et téléphone)
set -e
here=$(cd "$(dirname "$0")" && pwd)

command -v cargo >/dev/null || { echo "cargo introuvable : installe rust (sudo pacman -S rust)"; exit 1; }
(cd "$here/claude-usage" && cargo build --release --locked)
mkdir -p "$HOME/.local/bin"
install -m 755 "$here/claude-usage/target/release/claude-usage" "$HOME/.local/bin/claude-usage"

if kpackagetool6 --type Plasma/Applet --show org.glime.claudeusage >/dev/null 2>&1; then
  kpackagetool6 --type Plasma/Applet --upgrade "$here/plasmoid"
else
  kpackagetool6 --type Plasma/Applet --install "$here/plasmoid"
fi

conf="${XDG_CONFIG_HOME:-$HOME/.config}/claude-usage/config.json"
if [ ! -f "$conf" ]; then
  mkdir -p "$(dirname "$conf")"
  cp "$here/config.example.json" "$conf"
  echo "Config créée : $conf (mets-y ton topic ntfy pour les notifs téléphone)"
fi

echo
echo "Installé. Clic droit sur la barre des tâches > Ajouter des widgets > « Usage Claude »."
echo "Si le widget était déjà sur la barre : systemctl --user restart plasma-plasmashell"
"$HOME/.local/bin/claude-usage" --status || true
[ "$1" = "--test" ] && "$HOME/.local/bin/claude-usage" --test
exit 0
