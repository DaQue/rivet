#!/usr/bin/env bash
# Usage: robustness.sh   (RIVET=path/to/rivet to override). Exits 1 when rivet panics, hangs or is killed.
# Corrupt pieces of a repository and make sure rivet fails cleanly (never exit 101 = panic).
R=${RIVET:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/target/debug/rivet}
export GIT_CONFIG_GLOBAL=/dev/null GIT_AUTHOR_NAME=a GIT_AUTHOR_EMAIL=a@b GIT_COMMITTER_NAME=a GIT_COMMITTER_EMAIL=a@b
base=$(mktemp -d); cd $base
git init -q -b main src; cd src; seq 1 300 >f; echo a >g; git add -A; git commit -qm one; seq 1 300 | sed 's/^5$/x/' >f; git commit -qam two; git tag -a t -m t; git pack-refs --all; git repack -adq; git branch side; cd ..
bad=0
try() { # name, mutate-command
  rm -rf w; cp -a src w; (cd w; eval "$2") 2>/dev/null
  for cmd in "status" "log --oneline" "fsck" "branch -a" "diff" "show HEAD" "checkout side" "tag" "config -l" "ls-files" "cat-file -p HEAD" "reset --hard" "merge side" "commit -a -m x" "rev-parse HEAD" "verify-pack .git/objects/pack/*.idx"; do
    (cd w; timeout 20 $R $cmd >/dev/null 2>&1); rc=$?
    if [ $rc -eq 101 ] || [ $rc -eq 124 ] || [ $rc -ge 129 ]; then echo "PANIC/HANG rc=$rc: [$1] rivet $cmd"; bad=1; fi
  done
}
try index-trunc 'head -c 50 .git/index > i && mv i .git/index'
try index-garbage 'head -c 400 /dev/urandom > .git/index'
try index-flip 'printf "\377\377" | dd of=.git/index bs=1 seek=40 conv=notrunc'
try head-garbage 'echo "ref: refs/heads/../../x" > .git/HEAD'
try head-empty ': > .git/HEAD'
try head-binary 'head -c 100 /dev/urandom > .git/HEAD'
try packed-garbage 'head -c 300 /dev/urandom > .git/packed-refs'
try packed-badhex 'printf "zzzz refs/heads/x\n" > .git/packed-refs'
try config-garbage 'head -c 200 /dev/urandom > .git/config'
try config-unterminated 'printf "[core\n\tbare = " > .git/config'
try ref-garbage 'echo notahash > .git/refs/heads/main'
try ref-empty ': > .git/refs/heads/main'
try ref-loop 'echo "ref: refs/heads/a" > .git/refs/heads/main; echo "ref: refs/heads/main" > .git/refs/heads/a'
try pack-trunc 'p=$(ls .git/objects/pack/*.pack); head -c 200 $p > x && mv x $p'
try pack-flip 'p=$(ls .git/objects/pack/*.pack); printf "\377\377\377" | dd of=$p bs=1 seek=30 conv=notrunc'
try idx-trunc 'p=$(ls .git/objects/pack/*.idx); head -c 500 $p > x && mv x $p'
try idx-flip 'p=$(ls .git/objects/pack/*.idx); printf "\377\377\377" | dd of=$p bs=1 seek=1100 conv=notrunc'
try loose-garbage 'git unpack-objects -q < /dev/null; o=$(git rev-parse HEAD); git cat-file -p $o >/dev/null; rm .git/objects/pack/*; echo junk > .git/objects/junk'
try objects-missing 'rm -rf .git/objects/pack'
try worktree-file-dir 'rm g; mkdir g; echo x > g/y'
try merge-head-garbage 'echo zzz > .git/MERGE_HEAD'
try gitfile-garbage 'rm -rf .git; echo "gitdir: /nonexistent" > .git'
try reflog-garbage 'mkdir -p .git/logs/refs/heads; head -c 200 /dev/urandom > .git/logs/refs/heads/main'
if [ $bad = 0 ]; then echo ROBUST-OK; else exit 1; fi
