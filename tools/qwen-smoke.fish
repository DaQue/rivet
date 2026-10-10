#!/usr/bin/env fish
# Smoke test: the claude-qwen wrapper (via the :11499 shim) answers end to end.
if not functions -q claude-qwen
    source $HOME/.config/fish/config.fish
end
cd /home/davidq/Projects/rivet
claude-qwen -p "Reply with exactly: WRAPPER OK"
echo "QWEN_WRAPPER_EXIT=$status"
