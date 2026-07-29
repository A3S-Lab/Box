#!/bin/sh
# Install the a3s-box agent skill into a coding agent's skills directory.
# One SKILL.md, reused across every agent that speaks the Agent-Skills (SKILL.md)
# format. Symlinks by default (single source of truth); --copy to detach.
#
# Usage:
#   ./install.sh [--copy] [--home] <agent>...
#   ./install.sh --dir <path>            # install into an explicit skills dir
#   curl -fsSL <raw-install-url> | sh -s -- --home <agent>
#
#   agents:  agents  claude  codex  a3s-code  all
#     agents   -> .agents/skills   cross-tool standard: Codex, Gemini CLI, Amp,
#                                   Cursor, OpenCode, Zed all read this root
#     claude   -> .claude/skills   Claude Code/SDK, Cline, Cursor & OpenCode compat
#     codex    -> .codex/skills    Codex-specific; the a3s CLI menu also scans it
#     a3s-code -> .a3s/skills       a3s-code agent dir
#     all      -> agents + claude + codex + a3s-code
#   --home   install at user scope ($HOME) instead of the current project
#   --copy   copy the file instead of symlinking
#   --dir P  treat P as a skills root and drop a3s-box/SKILL.md inside it
#
# Examples:
#   ./install.sh all                     # wire every root in this repo
#   ./install.sh --home agents claude    # user-wide cross-tool + Claude Code
#   ./install.sh --dir ./my-agent/skills # any SKILL.md-format agent dir
#   curl .../install.sh | sh -s -- --home a3s-code
set -eu

SKILL_URL="${A3S_BOX_SKILL_URL:-https://raw.githubusercontent.com/A3S-Lab/Box/main/integrations/skills/a3s-box/SKILL.md}"
SRC=""
REMOTE_SOURCE=0
TEMP_SOURCE_DIR=""

if [ -f "$0" ]; then
  SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
  [ -f "$SCRIPT_DIR/a3s-box/SKILL.md" ] && SRC="$SCRIPT_DIR/a3s-box/SKILL.md"
fi

cleanup() {
  [ -n "$TEMP_SOURCE_DIR" ] && rm -rf "$TEMP_SOURCE_DIR"
}

if [ -z "$SRC" ]; then
  command -v curl >/dev/null 2>&1 || {
    echo "error: curl is required when install.sh is streamed" >&2
    exit 1
  }
  TEMP_SOURCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/a3s-box-skill.XXXXXX")"
  trap cleanup 0
  trap 'cleanup; exit 1' HUP INT TERM
  SRC="$TEMP_SOURCE_DIR/SKILL.md"
  curl --proto '=https' --tlsv1.2 -fsSL "$SKILL_URL" -o "$SRC"
  REMOTE_SOURCE=1
fi

grep -q '^name: a3s-box$' "$SRC" || {
  echo "error: downloaded file is not the a3s-box Skill" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
Usage:
  install.sh [--copy] [--home] <agent>...
  install.sh --dir <path>

Agents: agents | claude | codex | a3s-code | all
  --home   install in the user-level skills root
  --copy   copy SKILL.md instead of creating a symlink
  --dir P  install into an explicit skills root
USAGE
}

COPY=0; SCOPE=project; DIR=""; AGENTS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --copy) COPY=1 ;;
    --home) SCOPE=home ;;
    --dir)  shift; DIR="${1:?--dir needs a path}" ;;
    agents|claude|codex|a3s-code|all) AGENTS="$AGENTS $1" ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown arg '$1'" >&2; exit 1 ;;
  esac
  shift
done

# A streamed installer downloads SKILL.md into a temporary directory. Copy it
# so the installed skill remains valid after cleanup, even without --copy.
[ "$REMOTE_SOURCE" -eq 1 ] && COPY=1

# skills root for a named agent at the chosen scope
root_for() {
  base="."; [ "$SCOPE" = home ] && base="$HOME"
  case "$1" in
    agents)   echo "$base/.agents/skills" ;;  # cross-tool: Codex/Gemini/Amp/Cursor/OpenCode/Zed
    claude)   echo "$base/.claude/skills" ;;
    codex)    echo "$base/.codex/skills" ;;
    a3s-code) echo "$base/.a3s/skills" ;;      # agent-dir convention; pass --dir for a custom agent
  esac
}

place() {  # place <skills-root>
  dest="$1/a3s-box"
  mkdir -p "$dest"
  if [ "$COPY" -eq 1 ]; then
    cp "$SRC" "$dest/SKILL.md"; echo "copied   -> $dest/SKILL.md"
  else
    ln -sf "$SRC" "$dest/SKILL.md"; echo "linked   -> $dest/SKILL.md"
  fi
}

[ -n "$DIR" ] && { place "$DIR"; }

case "$AGENTS" in *all*) AGENTS="agents claude codex a3s-code" ;; esac
for a in $AGENTS; do place "$(root_for "$a")"; done

[ -z "$DIR$AGENTS" ] && { echo "nothing to do — pass an agent (agents|claude|codex|a3s-code|all) or --dir" >&2; exit 1; }
echo "done. reload the agent to pick up the skill."
