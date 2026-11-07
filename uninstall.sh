#!/bin/sh
# Retire le widget et le script (la config et l'état restent, à supprimer à la main si besoin).
kpackagetool6 --type Plasma/Applet --remove org.glime.claudeusage 2>/dev/null || true
rm -f "$HOME/.local/bin/claude-usage"
echo "Désinstallé. Restent : ~/.config/claude-usage/ et ~/.local/state/claude-usage/"
