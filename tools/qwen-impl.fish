#!/usr/bin/env fish
# qwen-impl.fish <task-file> [<task-name>]
# Runs ONE implementation task through the local qwen (claude-qwen -> shim :11499 -> Ollama).
# Logs the full transcript to ~/.hermes/cache/scratch/qwen-impl/<name>.log so a collapsed
# terminal result never loses the work, then prints the tail and a machine-readable exit line.
if test (count $argv) -lt 1
    echo "usage: qwen-impl.fish <task-file> [<task-name>]"
    exit 2
end
set task_file $argv[1]
set name (test (count $argv) -ge 2; and echo $argv[2]; or basename $task_file .md)
set logdir $HOME/.hermes/cache/scratch/qwen-impl
mkdir -p $logdir
set log $logdir/$name.log

if not functions -q claude-qwen
    source $HOME/.config/fish/config.fish
end
if not ss -ltn | grep -q ':11499'
    echo "shim not running on 11499 -- start it: qwen-shim-start"
    exit 3
end

cd /home/davidq/Projects/rivet
# Guardrails learned from T01's 20-minute runaway (32k output tokens, zero writes):
# the model's real window is 64k (not the 200k Claude Code assumes), extended
# thinking is off (a 3-bit 27B loops in CoT here), and a hard output ceiling makes
# any future spin-out fail fast instead of burning the turn.
set -gx CLAUDE_CODE_MAX_CONTEXT_TOKENS 65536
set -gx CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT 1
set -gx CLAUDE_CODE_MAX_OUTPUT_TOKENS 16000
set -gx MAX_THINKING_TOKENS 0
echo "=== qwen task: $name ==="
echo "=== start: "(date -Iseconds)" ==="
# T01 PROVED ASKING NICELY DOESN'T WORK: headless `claude -p` refuses every Edit/Write
# without a permission mode ("...you haven't granted it yet"). qwen produced a full
# 25,827-char hash.rs three times and all three writes were rejected, so the model looked
# broken while the harness was the broken part. acceptEdits auto-allows file edits; other
# tools stay gated. Flags go BEFORE -p because -p consumes the prompt.
# T02 PATCH: acceptEdits still left Bash gated, so qwen could not run cargo check/test itself
# and its "verified" claims came back unverifiable. --allowedTools adds Bash (recovery risk is
# covered by the baseline commit + backups/); --max-turns caps a spin-out the way
# CLAUDE_CODE_MAX_OUTPUT_TOKENS caps tokens.
# T01b PATCH: the session transcript proved the failure mode was not laziness but AMNESIA --
# over 60 turns qwen issued 80 Reads and 0 Edits, always on the same 4 files, with only four
# text blocks (all variants of "let me re-read the two target files"). The shim used to fight
# the 64K window by dropping oldest assistant/user pairs, which erased the evidence that it
# had already read anything. The shim now elides tool-result BODIES and injects a work digest
# instead, and the rules below state the loop-breaking workflow up front. The turn ceiling
# doubles because the carousel, not the work, is what consumed 60.
set promptfile $logdir/$name.prompt
cat $task_file > $promptfile
echo '' >> $promptfile
echo '---' >> $promptfile
echo 'HARNESS OPERATING RULES (mandatory; override any instinct to explore):' >> $promptfile
echo '* Work FILE BY FILE. For each target file: Read ONLY the line range you are about to change, then apply the Edit in the SAME turn. Never re-read a file you have already read.' >> $promptfile
echo '* The context window is 64K. Older tool results are replaced by a "[shim context note]" that lists what you already read and edited -- trust it and act on it instead of re-reading.' >> $promptfile
echo '* Replace EVERY todo!() in the target files, and never leave a stub behind. Scaffold signatures, types, lifetimes and doc comments are FROZEN: do not add, rename or remove public items, and do not add dependencies.' >> $promptfile
echo '* Finish by adding #[cfg(test)] tests that would fail if the logic were wrong, then run `cargo check --workspace --all-targets` and `cargo test -p rivet-core`.' >> $promptfile
# T01b2 PATCH: two runs in a row hit the 120-turn ceiling with the work ALREADY DONE (T01b2 had
# blob.rs implemented and the workspace compiling, then kept reading until the cap, and the
# harness reported failure). Give the model a stop condition it can recognise so "done" is a
# state, not a turn count.
echo '* STOP when done. DONE means: no todo!() left in your target files, `cargo check --workspace --all-targets` clean, `cargo test -p rivet-core` GREEN. Once that is true, stop touching files and write the report in your next message.' >> $promptfile
claude-qwen --permission-mode acceptEdits --allowedTools 'Read,Edit,Bash' --max-turns 120 --output-format stream-json --verbose -p "$(cat $promptfile)" 2>&1 | tee $log
set st $pipestatus[1]
echo "=== end: "(date -Iseconds)" ==="
cargo check --workspace --all-targets 2>&1 | tail -25
echo "QWEN_TASK=$name"
echo "QWEN_IMPL_EXIT=$st"
