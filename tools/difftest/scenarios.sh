#!/usr/bin/env bash
# The differential scenarios. Run all: tools/difftest/scenarios.sh; run some: ... name1 name2
FAKESSH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/fakessh"
AUTHSERVER="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/authserver.py"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
NOW0=$(date +%s)   # one wall-clock reading shared by both implementations

# Hooks: every hook in the list is the same script, which appends what it was given (argv, stdin,
# cwd, the GIT_* variables that matter) to `hooks.log` in the work tree, where the harness's
# work-tree comparison sees it. `hook-fail-<name>` (holding an exit code) makes that hook fail,
# `hook-append-<name>` appends its content to the message file the hook was handed.
hook_script='#!/bin/sh
n=$(basename "$0")
{
    echo "== $n [$*] cwd=$(basename "$(pwd)") idx=${GIT_INDEX_FILE-unset} prefix=${GIT_PREFIX-unset} editor=${GIT_EDITOR-unset} action=${GIT_REFLOG_ACTION-unset} dir=$(printf %s "${GIT_DIR-unset}" | sed "s#^$(pwd)/#./#") wt=${GIT_WORK_TREE-unset}"
    case $n in
        post-rewrite|pre-push) sed "s/^/  stdin: /" ;;
        prepare-commit-msg|commit-msg)
            # (a `hook-nocomments` file hides the "#" template lines git adds when it would start
            # an editor; rivet never does — see GAPS.md)
            if [ -f hook-nocomments ]; then grep -v "^#" "$1" | grep -v "^$" | sed "s/^/  msg: /"; else sed "s/^/  msg: /" "$1"; fi ;;
    esac
} >>hooks.log
echo "out:$n"
echo "err:$n" >&2
[ -f "hook-append-$n" ] && cat "hook-append-$n" >>"$1"
[ -f "hook-empty-$n" ] && : >"$1"
[ -f "hook-fail-$n" ] && exit "$(cat "hook-fail-$n")"
exit 0
'
# Put hooks.log into the transcript, so that a difference is readable in transcript.diff.
dump_hooklog() {
    echo "--- hooks.log" >>"$TRANSCRIPT"
    cat hooks.log >>"$TRANSCRIPT"
}
install_hooks() {
    local n
    mkdir -p .git/hooks
    for n in "$@"; do
        printf '%s' "$hook_script" >".git/hooks/$n"
        chmod +x ".git/hooks/$n"
    done
}

s_hooks_commit() {
    v init -q repo; cd repo
    mkdir sub
    echo a >a; v add a
    install_hooks pre-commit prepare-commit-msg commit-msg post-commit post-rewrite
    v commit -m one
    echo b >b; v add b
    (cd sub && v commit -m "from a subdirectory")
    echo c >c; v add c
    v commit --no-verify -m "no verify"
    # a failing hook aborts, in git's order, with the hook's output and exit status 1
    echo d >d; v add d
    echo 1 >hook-fail-pre-commit
    v commit -m "pre-commit fails"
    rm hook-fail-pre-commit
    echo 3 >hook-fail-prepare-commit-msg
    v commit -m "prepare fails"
    v commit --no-verify -m "prepare fails, no verify"
    rm hook-fail-prepare-commit-msg
    echo 2 >hook-fail-commit-msg
    v commit -m "commit-msg fails"
    v commit --no-verify -m "commit-msg skipped"
    v log --format=%s
    echo d2 >d; v add d
    echo 5 >hook-fail-post-commit
    v commit -m "post-commit fails"
    rm hook-fail-post-commit
    v log --format=%s
    # the hooks can rewrite the message
    echo e >e; v add e
    echo "added by prepare-commit-msg" >hook-append-prepare-commit-msg
    echo "added by commit-msg" >hook-append-commit-msg
    v commit -m "rewritten"
    rm hook-append-prepare-commit-msg hook-append-commit-msg
    v log -1 --format=%B
    echo f >f; v add f
    : >hook-empty-commit-msg
    v commit -m "emptied"
    rm hook-empty-commit-msg
    v status --short
    # nothing to commit: pre-commit still runs, nothing else does
    v reset -q --hard
    v commit -m "nothing"
    # amend: post-rewrite with the old and the new id on stdin
    v commit --amend -m "amended"
    v log --format=%s
    # a hook that is not executable is skipped, with a hint once per hook
    chmod -x .git/hooks/pre-commit .git/hooks/post-commit
    echo g >g; v add g
    v commit -m "not executable"
    echo h >h; v add h
    v commit -m "not executable again"
    git config advice.ignoredHook false
    echo i >i; v add i
    v commit -m "advice off"
    dump_hooklog
}

s_hooks_merge() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m base
    v branch ffside
    v switch -q -c side; echo s >s; v add s; tick; v commit -q -m side
    v switch -q ffside; echo f >f; v add f; tick; v commit -q -m ffside
    v switch -q main
    install_hooks pre-commit prepare-commit-msg commit-msg post-commit pre-merge-commit post-merge post-checkout
    rm -f hooks.log
    # fast-forward: only post-merge
    v merge ffside
    # already up to date: no hook at all
    v merge ffside
    # a real merge: pre-merge-commit, prepare-commit-msg (merge), commit-msg, post-merge, no post-commit
    tick; echo m >m; v add m; v commit -q -m main2
    v merge side -m "merge side"
    v log -1 --format=%B
    # --no-verify skips pre-merge-commit and commit-msg, not prepare-commit-msg
    v reset -q --hard HEAD~1
    v merge --no-verify side -m "merge side again"
    # the hooks can rewrite the message
    v reset -q --hard HEAD~1
    echo "from prepare" >hook-append-prepare-commit-msg
    echo "from commit-msg" >hook-append-commit-msg
    v merge side -m "rewritten merge"
    v log -1 --format=%B
    rm hook-append-prepare-commit-msg hook-append-commit-msg
    # pre-merge-commit fails: the merge stays staged and in progress
    v reset -q --hard HEAD~1
    echo 1 >hook-fail-pre-merge-commit
    v merge side -m "blocked by pre-merge-commit"
    v status --short
    v log --oneline
    v merge --abort
    rm hook-fail-pre-merge-commit
    for h in prepare-commit-msg commit-msg; do
        echo 2 >hook-fail-$h
        v merge side -m "blocked by $h"
        v status --short
        v merge --abort
        rm hook-fail-$h
    done
    # post-merge failing does not undo the merge
    echo 3 >hook-fail-post-merge
    v merge side -m "post-merge fails"
    v log --oneline
    rm hook-fail-post-merge
    # --no-commit runs no merge hooks; the commit that follows runs the commit hooks
    v reset -q --hard HEAD~1
    v merge --no-commit --no-ff side
    v commit -m "concluded"
    # a squash merge
    v reset -q --hard HEAD~1
    v merge --squash side
    v commit -m "squashed"
    # a conflicting merge: commit hooks only once it is resolved
    v switch -q -c c1 main~1; echo one >cf; v add cf; tick; v commit -q -m c1
    v switch -q -c c2 main~1; echo two >cf; v add cf; tick; v commit -q -m c2
    v merge c1
    echo resolved >cf; v add cf
    v commit -m "resolved"
    dump_hooklog
}

s_hooks_checkout() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v branch other
    v switch -q -c work; echo w >w; v add w; tick; v commit -q -m two
    v switch -q main
    install_hooks post-checkout
    rm -f hooks.log
    # every way of changing branch runs it with <old> <new> 1
    v switch work
    v checkout main
    v checkout -b topic
    v switch -c topic2
    v checkout --detach HEAD
    v checkout work
    v switch --detach main
    v switch -
    v checkout -B topic
    v switch -C topic2 work
    # staying where we are still counts
    v switch topic2
    v checkout topic2
    # paths: flag 0, old and new both HEAD
    echo changed >a
    v checkout -- a
    v checkout work -- w
    # a failing hook turns into the exit status but the checkout stays done
    echo 3 >hook-fail-post-checkout
    v switch main
    v branch --show-current
    v checkout work
    v checkout -b failing
    v checkout -- a
    rm hook-fail-post-checkout
    # --orphan and a bare switch back
    v checkout --orphan fresh
    v switch main
    # reset, merge and rebase moving HEAD
    v reset -q --hard work
    v merge -q work
    # not executable: hinted
    chmod -x .git/hooks/post-checkout
    v switch work
    dump_hooklog
}

s_hooks_hookspath() {
    v init -q repo; cd repo
    mkdir sub
    echo a >a; v add a
    install_hooks pre-commit commit-msg
    v commit -m "default hooks directory"
    # a relative core.hooksPath is taken from the work tree root, also from a subdirectory
    mkdir custom
    printf '%s' "$hook_script" >custom/pre-commit
    chmod +x custom/pre-commit
    v config core.hooksPath custom
    echo b >b; v add b
    (cd sub && v commit -m "relative path from a subdirectory")
    # ...and the hooks of the default directory are no longer consulted (commit-msg is not in custom)
    v config core.hooksPath ../custom
    echo c >c; v add c
    v commit -m "relative path that leads nowhere"
    # an absolute path
    v config core.hooksPath "$PWD/custom"
    echo d >d; v add d
    v commit -m "absolute path"
    # an empty value is not "unset": no hook is found at all
    v config core.hooksPath ""
    echo e >e; v add e
    v commit -m "empty value"
    # a hook that is not executable is skipped, with a hint naming the path as configured
    v config core.hooksPath custom
    chmod -x custom/pre-commit
    echo f >f; v add f
    v commit -m "not executable"
    chmod +x custom/pre-commit
    # a hook that fails, with the exit status of the hook
    echo 7 >hook-fail-pre-commit
    echo g >g; v add g
    v commit -m "failing"
    rm hook-fail-pre-commit
    # a hook that is a directory is no hook
    v config core.hooksPath dirhooks
    mkdir -p dirhooks/pre-commit
    v commit -m "directory"
    # a script without a #! line is run by the shell
    printf 'echo bare script >>hooks.log\n' >dirhooks/commit-msg
    rmdir dirhooks/pre-commit
    chmod +x dirhooks/commit-msg
    echo h >h; v add h
    v commit -m "no shebang"
    v log --format=%s
    dump_hooklog
}

s_hooks_rebase() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    v clone -q origin.git repo
    cd repo
    v switch -q -c topic
    for i in 1 2 3; do tick; echo t$i >t$i; v add t$i; v commit -q -m t$i; done
    v switch -q main
    tick; echo m >m; v add m; v commit -q -m main2
    install_hooks pre-rebase post-rewrite post-checkout pre-commit post-commit
    rm -f hooks.log
    v rebase main topic
    v log --format=%s
    # the branch is already on main: nothing is rewritten
    v rebase main
    # --onto, with the branch current
    v switch -q main
    tick; echo n >n; v add n; v commit -q -m main3
    v switch -q topic
    v rebase --onto main main~2
    v log --format=%s
    # fast-forwarding rebase
    v switch -q main
    v branch -f ahead topic
    v rebase ahead
    v log --format=%s -2
    # a refusing pre-rebase
    v switch -q topic
    tick; echo t4 >t4; v add t4; v commit -q -m t4
    echo 1 >hook-fail-pre-rebase
    v rebase main
    v status --short
    v rebase --no-verify main
    v log --format=%s
    rm hook-fail-pre-rebase
    # conflicts: post-rewrite once, at the end, with every commit
    v switch -q main; tick; echo x >c; v add c; v commit -q -m c-main
    v switch -q -c cf main~1; tick; echo y >c; v add c; v commit -q -m c-topic
    tick; echo z >z; v add z; v commit -q -m z-topic
    v rebase main
    echo merged >c; v add c
    v rebase --continue
    v log --format=%s
    # pull --rebase
    (cd ../seed && tick && echo p >p && g add p && g commit -qm upstream2 && g push -q origin HEAD:main 2>/dev/null)
    v switch -q main
    tick; echo l >l; v add l; v commit -q -m local-main
    v pull --rebase
    v log --format=%s
    dump_hooklog
}

s_hooks_push() {
    g init -q --bare origin.git
    g init -q --bare other.git
    v init -q repo; cd repo
    v remote add origin ../origin.git
    echo a >a; v add a; v commit -q -m one
    install_hooks pre-push
    rm -f hooks.log
    v push origin main
    # nothing to push: the hook still runs, with no lines
    v push origin main
    tick; v commit -q --allow-empty -m two
    v tag v1
    v push origin main:other v1
    v push origin HEAD:refs/heads/headref
    v push origin :other
    v push --delete origin v1
    v push --tags origin
    v push --dry-run origin main:dry
    v push --all origin
    # a url instead of a remote name
    v push ../other.git main:byurl
    # --no-verify skips it
    v push --no-verify origin main:nv
    # a refusing hook: nothing is sent, git's error, exit 1
    echo 1 >hook-fail-pre-push
    v push origin main:blocked
    v push --dry-run origin main:blocked2
    v push --no-verify origin main:allowed
    rm hook-fail-pre-push
    # a rejected non-fast-forward is not offered to the hook
    v reset -q --hard HEAD~1
    tick; v commit -q --allow-empty -m diverged
    v push origin main
    v push --force origin main
    vg --git-dir=../origin.git for-each-ref --format='%(refname)'
    dump_hooklog
}

s_hooks_pick() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v switch -q -c side
    for i in 1 2 3 4 5; do tick; echo s$i >s$i; v add s$i; v commit -q -m side$i; done
    tick; echo side >c; v add c; v commit -q -m c-side
    v switch -q main
    tick; echo main >c; v add c; v commit -q -m c-main
    install_hooks pre-commit prepare-commit-msg commit-msg post-commit post-rewrite
    rm -f hooks.log
    # a pick runs prepare-commit-msg (source "message") and post-commit; no pre-commit, no commit-msg
    v cherry-pick side~5
    v revert --no-edit HEAD
    # --no-commit runs nothing
    v cherry-pick -n side~4
    v reset -q --hard
    # the message prepare-commit-msg leaves is the one committed; a failing post-commit is ignored
    echo extra >hook-append-prepare-commit-msg
    echo 5 >hook-fail-post-commit
    v cherry-pick side~4
    v log -1 --format=%B
    rm hook-append-prepare-commit-msg hook-fail-post-commit
    # several commits: the hooks run once per commit, no post-rewrite
    v cherry-pick side~3 side~2
    v log --format=%s
    # a refusing prepare-commit-msg: error, status 1, the change stays staged for --continue
    echo 3 >hook-fail-prepare-commit-msg
    v cherry-pick side~1
    v status --short
    ls .git/CHERRY_PICK_HEAD
    rm hook-fail-prepare-commit-msg
    v cherry-pick --continue
    v log --format=%s -3
    echo 3 >hook-fail-prepare-commit-msg
    v revert --no-edit HEAD~1
    v status --short
    v revert --continue
    rm hook-fail-prepare-commit-msg
    v reset -q --hard
    # a conflict: --continue is a full commit, with pre-commit and commit-msg and source "merge"
    v cherry-pick side
    echo resolved >c; v add c
    echo 1 >hook-fail-pre-commit
    v cherry-pick --continue
    rm hook-fail-pre-commit
    echo 1 >hook-fail-commit-msg
    v cherry-pick --continue
    rm hook-fail-commit-msg
    v cherry-pick --continue
    v log --format=%s -2
    # the same for a revert
    tick; echo more >>c; v add c; v commit -q -m c-more
    v revert --no-edit HEAD~1
    echo fixed >c; v add c
    v revert --continue
    v log --format=%s -2
    dump_hooklog
}

s_hooks_am() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v switch -q -c topic
    for i in 1 2 3; do tick; echo p$i >p$i; v add p$i; v commit -q -m "patch $i"; done
    g format-patch -q -3 -o ../patches >/dev/null
    v switch -q main
    v branch -q -D topic
    install_hooks applypatch-msg pre-applypatch post-applypatch pre-commit prepare-commit-msg commit-msg post-commit post-rewrite
    rm -f hooks.log
    # am runs the three applypatch hooks per patch and none of the commit hooks
    v am ../patches/0001-patch-1.patch
    v log --format=%s
    # applypatch-msg may rewrite the message; a failing post-applypatch is ignored
    echo extra >hook-append-applypatch-msg
    echo 7 >hook-fail-post-applypatch
    v am ../patches/0002-patch-2.patch
    v log -1 --format=%B
    rm hook-append-applypatch-msg hook-fail-post-applypatch
    # --no-verify skips applypatch-msg and pre-applypatch, not post-applypatch
    v am --no-verify ../patches/0003-patch-3.patch
    v reset -q --hard HEAD~3
    # a refusing pre-applypatch: exit 1, the patch stays applied in the index, --continue commits it
    echo 1 >hook-fail-pre-applypatch
    v am ../patches/0001-patch-1.patch ../patches/0002-patch-2.patch
    v status --short
    v log --format=%s
    v am --continue
    v log --format=%s
    rm hook-fail-pre-applypatch
    v am --continue
    v log --format=%s
    v reset -q --hard HEAD~2
    # a refusing applypatch-msg stops before the patch is applied
    echo 1 >hook-fail-applypatch-msg
    v am ../patches/0001-patch-1.patch ../patches/0002-patch-2.patch
    v status --short
    v am --skip
    rm hook-fail-applypatch-msg
    v am --continue
    v am --skip
    v log --format=%s
    dump_hooklog
}

s_hooks_merge_continue() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m base
    v switch -q -c c1; echo one >cf; v add cf; tick; v commit -q -m c1
    v switch -q -c c2 main; echo two >cf; v add cf; tick; v commit -q -m c2
    install_hooks pre-commit prepare-commit-msg commit-msg post-commit pre-merge-commit post-merge post-rewrite
    rm -f hooks.log
    touch hook-nocomments
    # --continue concludes with the hooks of `git commit` for a merge, source "merge"
    v merge c1
    echo resolved >cf; v add cf
    v merge --continue
    v log --format=%s -3
    v reset -q --hard HEAD~1
    # the hooks may rewrite the message
    v merge c1
    echo resolved >cf; v add cf
    echo "from prepare" >hook-append-prepare-commit-msg
    echo "from commit-msg" >hook-append-commit-msg
    v merge --continue
    v log -1 --format=%B
    rm hook-append-prepare-commit-msg hook-append-commit-msg
    v reset -q --hard HEAD~1
    # each refusing hook leaves the merge in progress
    v merge c1
    echo resolved >cf; v add cf
    for h in pre-commit prepare-commit-msg commit-msg; do
        echo 2 >hook-fail-$h
        v merge --continue
        v status --short
        rm hook-fail-$h
    done
    echo 2 >hook-fail-post-commit
    v merge --continue
    rm hook-fail-post-commit
    v log --format=%s -3
    dump_hooklog
}

s_hooks_stash() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    install_hooks pre-commit prepare-commit-msg commit-msg post-commit post-rewrite post-checkout post-merge
    rm -f hooks.log
    # push, apply, pop, drop and -u create and restore commits without running any hook
    echo dirty >>a; echo u >untracked
    v stash push -u -m first
    v stash apply
    v checkout -q -- a; rm untracked
    v stash pop
    v stash drop
    v checkout -q -- a; rm -f untracked
    v stash list
    # (`stash branch` runs post-checkout in git; rivet has no `stash branch` — see GAPS.md)
    echo dirty >>a
    v stash push -q
    v stash list
    v stash pop -q
    v status --short
    dump_hooklog
}

s_hooks_clone() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g branch other && tick && echo b >b && g add b && g commit -qm two && g push -q origin HEAD:main other 2>/dev/null)
    g init -q --bare empty.git
    # post-checkout from a hooks directory in the global config: a clone has no hooks of its own
    mkdir hooks hooks-fail
    printf '%s' "$hook_script" >hooks/post-checkout
    printf '#!/bin/sh\necho failing >&2\nexit 3\n' >hooks-fail/post-checkout
    chmod +x hooks/post-checkout hooks-fail/post-checkout
    g config --file "$GIT_CONFIG_GLOBAL" core.hooksPath "$PWD/hooks"
    # a clone runs it in the new work tree: <null> <tip> 1, GIT_DIR set, GIT_PREFIX not
    v clone -q origin.git c1
    v clone -q -b other origin.git c2
    # nothing is checked out in these: no hook
    v clone -q --bare origin.git bare.git
    v clone -q empty.git c3
    ls c3
    # the hook's status is the clone's
    g config --file "$GIT_CONFIG_GLOBAL" core.hooksPath "$PWD/hooks-fail"
    v clone -q origin.git c4
    g config --file "$GIT_CONFIG_GLOBAL" core.hooksPath "$PWD/hooks"
    # worktree add: <null> <commit> 1 in the new work tree, GIT_PREFIX empty, no GIT_DIR
    cd c1
    v worktree add -q ../wt1 -b wt1
    v worktree add -q ../wt2 -b wt2 main
    v worktree add -q --detach ../wt3 main~1
    v worktree add -q --no-checkout ../wt4 -b wt4
    g config --file "$GIT_CONFIG_GLOBAL" core.hooksPath "$ROOT/hooks-fail"
    v worktree add -q ../wt5 -b wt5
    cd ..
    for log in c1 c2 c4 wt1 wt2 wt3; do
        echo "--- $log/hooks.log" >>"$TRANSCRIPT"
        sed -e "s#$ROOT#<ROOT>#g" "$log/hooks.log" >>"$TRANSCRIPT"
    done
    ls wt4 wt5
}

# Sparse checkout: a tree with directories at several depths, and a dump of what a sparse checkout
# changes: the files present, the skip-worktree flags (read by real git), both configuration files
# and the pattern file.
sparse_fixture() {
    v init -q repo; cd repo
    mkdir -p a/sub b c/d/e f
    echo r1 >r1; echo r2 >r2; echo a1 >a/a1; echo as >a/sub/as; echo b1 >b/b1
    echo cd >c/d/cd; echo ce >c/d/e/ce; echo f1 >f/f1; echo c0 >c/c0
    v add .; tick; v commit -q -m one
}
dump_sparse() {
    {
        echo "--- sparse: $*"
        find . -path ./.git -prune -o -type f -print | sort
        g ls-files -t
        cat .git/config.worktree 2>&1
        cat .git/info/sparse-checkout 2>&1
    } >>"$TRANSCRIPT"
}

s_sparse_cone() {
    sparse_fixture
    v sparse-checkout list
    v sparse-checkout init
    dump_sparse init
    v sparse-checkout list
    v sparse-checkout set a b
    dump_sparse "set a b"
    v sparse-checkout list
    v sparse-checkout add c/d
    dump_sparse "add c/d"
    v sparse-checkout reapply
    v sparse-checkout set --cone c/d/e a/sub
    dump_sparse "set c/d/e a/sub"
    v sparse-checkout list
    v sparse-checkout set c c/d
    dump_sparse "set c c/d"
    v sparse-checkout set nothing/here a/
    dump_sparse "set nothing/here a/"
    v sparse-checkout set
    dump_sparse "set (nothing)"
    printf 'a\nb\n' | v sparse-checkout set --stdin
    printf 'c/d\n' | v sparse-checkout add --stdin
    v sparse-checkout list
    v sparse-checkout set ./f a//sub
    v sparse-checkout list
    v sparse-checkout disable
    dump_sparse disable
    v sparse-checkout list
    v sparse-checkout disable
    v sparse-checkout reapply
    v sparse-checkout add a
}

s_sparse_errors() {
    sparse_fixture
    v sparse-checkout
    v sparse-checkout bogus
    v sparse-checkout set --bogus
    # a refused path still leaves sparse checkout switched on, without a pattern file
    v sparse-checkout set 'a*'
    dump_sparse "set a*"
    v sparse-checkout list
    v sparse-checkout set /a
    v sparse-checkout set r1 a
    v sparse-checkout set --skip-checks r1 'a*'
    dump_sparse "set --skip-checks"
    v sparse-checkout list
    v sparse-checkout set a
    dump_sparse "set a"
    v sparse-checkout disable
    v sparse-checkout set b
    dump_sparse "set b after disable"
}

s_sparse_noncone() {
    sparse_fixture
    v sparse-checkout init --no-cone
    dump_sparse "init --no-cone"
    v sparse-checkout init
    v sparse-checkout set --no-cone '/a/*' '!/a/sub/' '/r*'
    dump_sparse "set --no-cone"
    v sparse-checkout list
    v sparse-checkout add '/b/'
    dump_sparse "add /b/"
    # without a flag the mode stays
    v sparse-checkout set f
    dump_sparse "set f"
    v sparse-checkout set --no-cone '/c/' '!/c/d/' '/c/d/e/'
    dump_sparse "nested include and exclude"
    v sparse-checkout set --no-cone '*.txt' 'b1'
    dump_sparse "unanchored"
    v sparse-checkout reapply --no-cone
    v sparse-checkout disable
    dump_sparse disable
}

s_sparse_dirty() {
    sparse_fixture
    v sparse-checkout set a b
    # a modified file outside the new definition stays, with a warning; untracked files stay too
    echo changed >>b/b1; echo new >b/new
    v sparse-checkout set a
    dump_sparse "set a with b/b1 modified"
    v status --short
    v sparse-checkout set a b
    dump_sparse "set a b again"
    cat b/b1
    # a deleted file outside the definition is just skipped
    rm a/a1
    v sparse-checkout set b
    dump_sparse "set b with a/a1 deleted"
    v sparse-checkout reapply
    v sparse-checkout disable
    dump_sparse disable
    v status --short
}

s_sparse_worktree() {
    sparse_fixture
    v branch other
    v sparse-checkout set a b
    # nothing is reported for the files outside the definition
    v status --short
    v diff --stat
    echo edit >>a/a1
    v diff --stat
    v add -u
    v status --short
    v commit -q -m "edit a1"
    v diff HEAD~1 --stat
    echo more >>b/b1
    v commit -q -a -m "edit b1"
    v show --stat --format=%s HEAD
    v ls-files
    v ls-files -s
    dump_sparse "after commits"
    # branches: files in the definition are updated, new ones outside are only recorded
    v switch -q -c topic
    echo a2 >a/a2; mkdir -p c/d f; echo c2 >c/d/c2; echo f2 >f/f2
    v add -f a/a2 c/d/c2 f/f2
    v commit -q -m "add files"
    dump_sparse "on topic"
    v switch -q main
    dump_sparse "back on main"
    v switch -q topic
    dump_sparse "topic again"
    v checkout -q main
    v reset -q --hard topic
    dump_sparse "reset --hard topic"
    v reset -q --hard HEAD~1
    dump_sparse "reset --hard HEAD~1"
    # a file that exists outside the definition and in the way
    v switch -q topic
    v sparse-checkout set a
    v switch -q main
    v sparse-checkout disable
    dump_sparse disable
    v status --short
}

s_sparse_add_rm() {
    sparse_fixture
    v sparse-checkout set a b
    mkdir -p f c/d
    echo n >f/new; echo m >c/d/newc; echo x >a/new; echo y >b/untracked
    # untracked files outside the definition are not added; the others are, and the status is 1
    v add .
    v status --short
    v add -A
    v reset -q
    v add f
    v add f/new c/d/newc
    v add c/c0
    v add -u f/f1
    v add -u c
    v add -n c/c0
    v add -n .
    v add nothing
    v add --sparse f/new
    v status --short
    dump_sparse "after add"
    v rm c/c0
    v rm --cached c/c0
    v rm -f f
    v rm a/a1 c/c0
    v rm -n c/c0
    v rm nothing
    v rm -r --cached a c
    v status --short
    git_config_advice=1
    v config advice.updateSparsePath false
    v add c/c0
    v rm c/c0
    v add -u
    dump_sparse "after rm"
}

s_sparse_history() {
    sparse_fixture
    v switch -q -c side
    for f in a/a1 c/c0 f/f1 r1; do tick; echo "side $f" >>$f; v add -f $f 2>/dev/null; done
    tick; echo new >a/anew; echo newc >c/cnew; v add a/anew c/cnew
    v commit -q -m "side changes"
    v switch -q main
    tick; echo main >>b/b1; v commit -q -a -m "main b1"
    v sparse-checkout set a b
    dump_sparse "sparse on main"
    # checkout of a path outside is not a known path
    v checkout -- c/c0
    v restore c/c0
    # merge with both sides moved: both sides moved
    v merge -q side -m "merge side"
    dump_sparse "after merge"
    v log --oneline
    v status --short
    # cherry-pick, revert and stash
    v reset -q --hard HEAD~1
    v cherry-pick side
    dump_sparse "after cherry-pick"
    v revert --no-edit HEAD
    echo wip >>a/a1; v stash push -q
    v stash pop -q
    v status --short
    dump_sparse "after stash"
    v diff --stat HEAD~2
    v commit -q -a -m wip
    v rebase side
    dump_sparse "after rebase"
    v branch -q old HEAD~1
    v checkout -q old
    dump_sparse "on old"
}

s_sparse_misc() {
    sparse_fixture
    v sparse-checkout set a b
    mkdir -p f; echo n >f/new
    v grep -n 1
    v grep -n c0
    v clean -n
    v clean -nd
    v mv a/a1 a/a9
    v mv c/c0 c/c9
    v reset -q -- c/c0
    v status --short
    v ls-files -t
    v ls-files -s c
    v log --stat --format=%s
    v show --stat --format=%s HEAD
    v diff HEAD~0 --stat
    v blame c/c0
    v archive --format=tar HEAD -o ../x.tar
    v describe --always
    v stash list
    v branch topic
    v switch -q topic
    echo t >>a/a9; v commit -q -a -m t
    v switch -q main
    v merge -q topic
    v status --short
    dump_sparse "end"
}

s_sparse_linked() {
    sparse_fixture
    v sparse-checkout set a
    # a linked work tree has its own definition
    v worktree add -q ../wt -b wtb
    (cd ../wt && v sparse-checkout list)
    (cd ../wt && v sparse-checkout set b f && g ls-files -t | tr '\n' ' ' >>"$TRANSCRIPT")
    echo >>"$TRANSCRIPT"
    (cd ../wt && v sparse-checkout list)
    v sparse-checkout list
    dump_sparse "main after"
    (cd ../wt && find . -type f -not -path './.git' | sort >>"$TRANSCRIPT")
    (cd ../wt && v sparse-checkout disable)
    (cd ../wt && find . -type f -not -path './.git' | sort >>"$TRANSCRIPT")
    v sparse-checkout list
}

s_hooks_run() {
    v init -q repo; cd repo
    mkdir sub
    install_hooks pre-commit post-rewrite
    printf 'one\ntwo\n' >stdin.txt
    v hook run pre-commit
    v hook run pre-commit -- a "b c"
    (cd sub && v hook run pre-commit -- from-sub)
    v hook run post-rewrite --to-stdin=stdin.txt -- amend
    v hook run --to-stdin=missing.txt post-rewrite
    echo 3 >hook-fail-pre-commit
    v hook run pre-commit
    rm hook-fail-pre-commit
    v hook run post-commit
    v hook run --ignore-missing post-commit
    v hook run not-a-hook
    v hook run --allow-unknown-hook-name not-a-hook
    v hook run --allow-unknown-hook-name --ignore-missing not-a-hook
    v hook run
    v hook
    chmod -x .git/hooks/pre-commit
    v hook run pre-commit
    v hook run --ignore-missing pre-commit
    v config core.hooksPath custom
    mkdir custom
    printf '%s' "$hook_script" >custom/pre-commit
    chmod +x custom/pre-commit
    v hook run pre-commit -- via-hookspath
    dump_hooklog
}

s_rev_parse_dirs() {
    v init -q repo; cd repo
    echo a >f; v add f; tick; v commit -q -m one
    mkdir sub
    v rev-parse --git-common-dir
    v rev-parse --git-dir
    v rev-parse --git-dir --git-common-dir
    v rev-parse --git-common-dir HEAD
    (cd sub && v rev-parse --git-common-dir)
    (cd sub && v rev-parse --git-dir)
    v worktree add -q ../wt -b wtb
    (cd ../wt && v rev-parse --git-common-dir)
    (cd ../wt && v rev-parse --git-dir)
    (cd ../wt && v rev-parse --git-dir --git-common-dir --is-inside-work-tree)
    v rev-parse --git-common-dir nonexistent
    cd .. && v init -q --bare bare.git
    (cd bare.git && v rev-parse --git-common-dir)
    (cd bare.git && v rev-parse --git-dir)
}

s_hooks_list() {
    v init -q repo; cd repo
    mkdir sub
    v hook list pre-commit
    install_hooks pre-commit
    v hook list pre-commit
    v hook list -z pre-commit
    v hook list --show-scope pre-commit
    (cd sub && v hook list pre-commit)
    v hook list post-commit
    v hook list not-a-hook
    v hook list --allow-unknown-hook-name not-a-hook
    v hook list
    v hook list pre-commit post-commit
    chmod -x .git/hooks/pre-commit
    v hook list pre-commit
    mkdir custom
    printf '%s' "$hook_script" >custom/pre-commit
    chmod +x custom/pre-commit
    v config core.hooksPath custom
    v hook list pre-commit
    v config core.hooksPath empty-dir
    v hook list pre-commit
    dump_hooklog
}

# Credentials: an HTTP server that wants Basic auth (alice / s3cret), helpers that log what they are
# told into `helper.log`, and the server's own request log. Both logs go into the transcript.
cred_helper() {  # <root> <name> <user> <password>: a helper that answers `get` with that credential
    local root=$1 name=$2 user=$3 password=$4
    cat >"$root/$name.sh" <<EOF
#!/bin/sh
echo "== $name \$1" >>"$root/helper.log"
sed 's/^/  in: /' >>"$root/helper.log"
if [ "\$1" = get ] && [ -n "$user" ]; then echo username=$user; echo password=$password; fi
EOF
    chmod +x "$root/$name.sh"
}
cred_dump() {
    local root=$1
    echo "--- helper.log" >>"$TRANSCRIPT"
    cat "$root/helper.log" >>"$TRANSCRIPT" 2>/dev/null
    echo "--- requests" >>"$TRANSCRIPT"
    # (how many upload-pack POSTs and repeated advertisements there are is not about credentials:
    # git sends one more POST per fetch than rivet, and rivet asks a push for two advertisements)
    grep -v 'POST .*upload-pack' "$root/req.log" | uniq >>"$TRANSCRIPT" 2>/dev/null
    rm -f "$root/helper.log" "$root/req.log"
}

s_credential() {
    local port=18793 root=$PWD
    mkdir -p repos
    g init -q --bare repos/o.git; g -C repos/o.git config http.receivepack true
    g clone -q repos/o.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    python3 "$AUTHSERVER" $port "$root/repos" alice s3cret "$root/req.log" >/dev/null 2>&1 &
    local server=$!
    sleep 1
    local url=http://127.0.0.1:$port/o.git
    export GIT_TERMINAL_PROMPT=0
    cred_helper "$root" good alice s3cret
    cred_helper "$root" bad alice wrong
    cred_helper "$root" none "" ""
    cred_helper "$root" second alice s3cret
    # no helper, no prompt
    v clone -q $url c0; cred_dump "$root"
    # a helper that knows the password
    v config --global credential.helper "$root/good.sh"
    v clone -q $url c_ok; cred_dump "$root"
    # a helper with the wrong password: erased, and git's message
    v config --global credential.helper "$root/bad.sh"
    v clone -q $url c1; cred_dump "$root"
    # helpers are tried in order; the first complete answer wins, store goes to all of them
    v config --global --add credential.helper "$root/good.sh"
    v clone -q $url c2; cred_dump "$root"
    v config --global --unset-all credential.helper
    v config --global credential.helper "$root/none.sh"
    v config --global --add credential.helper "$root/second.sh"
    v clone -q $url c3; cred_dump "$root"
    # an empty value resets the list
    v config --global --unset-all credential.helper
    v config --global credential.helper "$root/bad.sh"
    v config --global --add credential.helper ""
    v config --global --add credential.helper "$root/good.sh"
    v clone -q $url c4; cred_dump "$root"
    # a helper for one URL (configuration order, not specificity), and one for another host
    v config --global --unset-all credential.helper
    v config --global "credential.http://127.0.0.1:$port.helper" "$root/good.sh"
    v config --global "credential.http://localhost:$port.helper" "$root/bad.sh"
    v config --global "credential.https://127.0.0.1:$port.helper" "$root/bad.sh"
    v clone -q $url c5; cred_dump "$root"
    v config --global --unset-all "credential.http://127.0.0.1:$port.helper"
    v config --global "credential.http://127.0.0.1:$port/o.git.helper" "$root/good.sh"
    v clone -q $url c6; cred_dump "$root"
    v config --global --unset-all "credential.http://127.0.0.1:$port/o.git.helper"
    v config --global "credential.http://127.0.0.1:$port/other.helper" "$root/good.sh"
    v clone -q $url c7; cred_dump "$root"
    v config --global --unset-all "credential.http://127.0.0.1:$port/other.helper"
    # a shell-function helper
    v config --global credential.helper '!f() { echo "== shell $1" >>'"$root"'/helper.log; cat >/dev/null; test "$1" = get && echo username=alice && echo password=s3cret; }; f'
    v clone -q $url c8; cred_dump "$root"
    # useHttpPath and a configured user name are part of what the helper is told
    v config --global credential.helper "$root/good.sh"
    v config --global credential.useHttpPath true
    v clone -q $url c9; cred_dump "$root"
    v config --global --unset credential.useHttpPath
    v config --global credential.username alice
    v clone -q $url c10; cred_dump "$root"
    v config --global --unset credential.username
    # credentials in the URL are used after the 401, never asked of the helpers, then stored
    v clone -q http://alice:s3cret@127.0.0.1:$port/o.git c11; cred_dump "$root"
    v clone -q http://alice:wrong@127.0.0.1:$port/o.git c12; cred_dump "$root"
    # a prompt: through the askpass program, then handed to the helpers for storage
    v config --global credential.helper "$root/none.sh"
    printf '#!/bin/sh\necho "ask: $1" >>%s/helper.log\ncase "$1" in Username*) echo alice;; *) echo s3cret;; esac\n' "$root" >"$root/askpass.sh"
    chmod +x "$root/askpass.sh"
    GIT_ASKPASS="$root/askpass.sh" v clone -q $url c13; cred_dump "$root"
    # fetch and push in an existing repository
    v config --global credential.helper "$root/good.sh"
    (cd c_ok && tick && echo b >b && v add b && v commit -q -m two && v push origin HEAD:refs/heads/main && v fetch)
    cred_dump "$root"
    (cd c_ok && v push origin HEAD:refs/heads/second)
    cred_dump "$root"
    kill $server 2>/dev/null; wait $server 2>/dev/null
}

# A server that leaves discovery public and challenges only the POSTs (fetch, push, ls-refs): the 401
# arrives mid-command, the helpers are asked then, and the POST is sent again with the credentials.
s_credential_post() {
    local port=18795 root=$PWD
    mkdir -p repos
    g init -q --bare repos/o.git; g -C repos/o.git config http.receivepack true
    g -C repos/o.git symbolic-ref HEAD refs/heads/main
    g clone -q repos/o.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    AUTH_ONLY_POST=1 python3 "$AUTHSERVER" $port "$root/repos" alice s3cret "$root/req.log" >/dev/null 2>&1 &
    local server=$!
    sleep 1
    local url=http://127.0.0.1:$port/o.git
    export GIT_TERMINAL_PROMPT=0
    cred_helper "$root" good alice s3cret
    cred_helper "$root" bad alice wrong
    cred_post_dump() {
        echo "--- helper.log" >>"$TRANSCRIPT"
        cat "$root/helper.log" >>"$TRANSCRIPT" 2>/dev/null
        echo "--- requests (distinct)" >>"$TRANSCRIPT"
        sort -u "$root/req.log" >>"$TRANSCRIPT" 2>/dev/null
        rm -f "$root/helper.log" "$root/req.log"
    }
    # no helper: the POST is challenged and nothing can answer
    v clone -q $url c0; cred_post_dump
    # a helper that knows the password: asked at the POST, stored after it worked
    v config --global credential.helper "$root/good.sh"
    v clone -q $url c_post; cred_post_dump
    # a helper with the wrong password: rejected, erased, git's message
    v config --global credential.helper "$root/bad.sh"
    v clone -q $url c1; cred_post_dump
    # push and fetch in an existing repository
    v config --global credential.helper "$root/good.sh"
    (cd c_post && tick && echo b >b && v add b && v commit -q -m two && v push origin HEAD:refs/heads/main); cred_post_dump
    (cd seed && tick && echo c >c && g add c && g commit -qm three && g push -q origin HEAD:refs/heads/side 2>/dev/null)
    (cd c_post && v fetch origin); cred_post_dump
    # the same refusals on a fetch of an existing repository
    (cd seed && tick && echo d >d && g add d && g commit -qm four && g push -q origin HEAD:refs/heads/side 2>/dev/null)
    v config --global credential.helper "$root/bad.sh"
    (cd c_post && v fetch origin); cred_post_dump
    v config --global --unset-all credential.helper
    (cd c_post && v fetch origin); cred_post_dump
    kill $server 2>/dev/null; wait $server 2>/dev/null
}

# Helper output beyond username/password: `password_expiry_utc`, `oauth_refresh_token` (handed back to
# store/erase), an expired password (discarded, then the next helper or the prompt), and `quit=1`.
cred_helper_out() {  # <root> <name> <shell commands run on get>
    local root=$1 name=$2
    cat >"$root/$name.sh" <<HELPER_END
#!/bin/sh
echo "== $name \$1" >>"$root/helper.log"
sed 's/^/  in: /' >>"$root/helper.log"
if [ "\$1" = get ]; then $3; fi
HELPER_END
    chmod +x "$root/$name.sh"
}

s_credential_keys() {
    local port=18794 root=$PWD
    mkdir -p repos
    g init -q --bare repos/o.git; g -C repos/o.git config http.receivepack true
    g clone -q repos/o.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    python3 "$AUTHSERVER" $port "$root/repos" alice s3cret "$root/req.log" >/dev/null 2>&1 &
    local server=$!
    sleep 1
    local url=http://127.0.0.1:$port/o.git
    export GIT_TERMINAL_PROMPT=0
    cred_helper "$root" good alice s3cret
    cred_helper_out "$root" exp 'echo username=alice; echo password=s3cret; echo password_expiry_utc=4102444800; echo oauth_refresh_token=rt1'
    cred_helper_out "$root" stale 'echo username=alice; echo password=wrong; echo password_expiry_utc=4102444800; echo oauth_refresh_token=rt2'
    cred_helper_out "$root" old 'echo username=alice; echo password=s3cret; echo password_expiry_utc=1000'
    cred_helper_out "$root" quit 'echo quit=1'
    cred_helper_out "$root" quitfull 'echo username=alice; echo password=s3cret; echo quit=1'
    # the expiry and the refresh token come back on store
    v config --global credential.helper "$root/exp.sh"
    v clone -q $url c_exp; cred_dump "$root"
    # ... and on erase when the server refuses the password
    v config --global credential.helper "$root/stale.sh"
    v clone -q $url c_stale; cred_dump "$root"
    # an expired password is dropped (the user name stays) and the next helper is asked
    v config --global credential.helper "$root/old.sh"
    v config --global --add credential.helper "$root/good.sh"
    v clone -q $url c_old; cred_dump "$root"
    # nobody else to ask: the prompt names the user
    v config --global --unset-all credential.helper
    v config --global credential.helper "$root/old.sh"
    v clone -q $url c_old2; cred_dump "$root"
    # quit stops the helpers that follow, but not when the credential is already complete
    v config --global --unset-all credential.helper
    v config --global credential.helper "$root/quit.sh"
    v config --global --add credential.helper "$root/good.sh"
    v clone -q $url c_quit; cred_dump "$root"
    v config --global --unset-all credential.helper
    v config --global credential.helper "$root/quitfull.sh"
    v config --global --add credential.helper "$root/good.sh"
    v clone -q $url c_quitfull; cred_dump "$root"
    kill $server 2>/dev/null; wait $server 2>/dev/null
}

# `authtype`/`credential` from a helper: the request carries `Authorization: <authtype> <credential>`
# (a Bearer server here), `store` and `erase` hear the pair back, with `capability[]=authtype` first
# only when the helper announced it. No `continue`/`state` round yet.
s_credential_authtype() {
    local port=18795 root=$PWD
    mkdir -p repos
    g init -q --bare repos/o.git; g -C repos/o.git config http.receivepack true
    g clone -q repos/o.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    AUTH_BEARER=tok123 python3 "$AUTHSERVER" $port "$root/repos" x y "$root/req.log" >/dev/null 2>&1 &
    local server=$!
    sleep 1
    local url=http://127.0.0.1:$port/o.git
    export GIT_TERMINAL_PROMPT=0
    cred_helper_out "$root" bearer 'echo capability[]=authtype; echo authtype=Bearer; echo credential=tok123'
    cred_helper_out "$root" bare 'echo authtype=Bearer; echo credential=tok123'
    cred_helper_out "$root" badtok 'echo capability[]=authtype; echo authtype=Bearer; echo credential=nope'
    cred_helper_out "$root" basic 'echo username=alice; echo password=s3cret'
    v config --global credential.helper "$root/bearer.sh"
    v clone -q $url c_bearer; cred_dump "$root"
    # a helper that does not announce the capability still gets its header sent, and hears nothing of it
    v config --global credential.helper "$root/bare.sh"
    v clone -q $url c_bare; cred_dump "$root"
    # the server refuses the token: erase, and the failure is git's
    v config --global credential.helper "$root/badtok.sh"
    v clone -q $url c_bad; cred_dump "$root"
    # the pair is complete, so the helper that follows is never asked
    v config --global credential.helper "$root/bearer.sh"
    v config --global --add credential.helper "$root/basic.sh"
    v clone -q $url c_first; cred_dump "$root"
    kill $server 2>/dev/null; wait $server 2>/dev/null
}

# `url=` from a helper replaces the whole credential (protocol, host, path, user, password, all
# percent-decoded); what was set before it is dropped, the helpers after it see the new credential,
# `store`/`erase` hear it, and an unparseable URL is fatal.
s_credential_url() {
    local port=18796 root=$PWD
    mkdir -p repos
    g init -q --bare repos/o.git; g -C repos/o.git config http.receivepack true
    g clone -q repos/o.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    python3 "$AUTHSERVER" $port "$root/repos" alice s3cret "$root/req.log" >/dev/null 2>&1 &
    local server=$!
    sleep 1
    local url=http://127.0.0.1:$port/o.git
    export GIT_TERMINAL_PROMPT=0
    cred_helper "$root" good alice s3cret
    cred_helper_out "$root" same "echo url=http://alice:s3cret@127.0.0.1:$port/o.git/"
    cred_helper_out "$root" moved "echo url=http://%61lice:s3%63ret@other.example:8080//x%20y/z"
    cred_helper_out "$root" cleared "echo username=alice; echo url=http://127.0.0.1:$port"
    cred_helper_out "$root" junk 'echo url=nonsense'
    cred_helper_out "$root" nl 'echo url=http://h/a%0ab'
    v config --global credential.helper "$root/same.sh"
    v clone -q $url c_same; cred_dump "$root"
    v config --global credential.helper "$root/moved.sh"
    v clone -q $url c_moved; cred_dump "$root"
    # the user name given before the url is gone; the next helper is asked about the new credential
    v config --global credential.helper "$root/cleared.sh"
    v config --global --add credential.helper "$root/good.sh"
    v clone -q $url c_cleared; cred_dump "$root"
    v config --global --unset-all credential.helper
    v config --global credential.helper "$root/junk.sh"
    v clone -q $url c_junk; cred_dump "$root"
    v config --global credential.helper "$root/nl.sh"
    v clone -q $url c_nl; cred_dump "$root"
    kill $server 2>/dev/null; wait $server 2>/dev/null
}

# Helper output `state[]` and `continue`: kept only when the helper announced `capability[]=state`,
# then handed back (with that capability line) to `store` and `erase`, after `wwwauth[]`.
s_credential_state() {
    local port=18797 port2=18798 root=$PWD
    mkdir -p repos
    g init -q --bare repos/o.git; g -C repos/o.git config http.receivepack true
    g clone -q repos/o.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    AUTH_BEARER=tok123 python3 "$AUTHSERVER" $port "$root/repos" x y "$root/req.log" >/dev/null 2>&1 &
    local server=$!
    python3 "$AUTHSERVER" $port2 "$root/repos" alice s3cret "$root/req2.log" >/dev/null 2>&1 &
    local server2=$!
    sleep 1
    export GIT_TERMINAL_PROMPT=0
    cred_helper_out "$root" both 'echo capability[]=authtype; echo capability[]=state; echo authtype=Bearer; echo credential=tok123; echo state[]=foo:bar; echo continue=1'
    cred_helper_out "$root" rejected 'echo capability[]=authtype; echo capability[]=state; echo authtype=Bearer; echo credential=bad; echo state[]=s1; echo state[]=s2'
    cred_helper_out "$root" stateonly 'echo capability[]=state; echo username=alice; echo password=s3cret; echo state[]=d1'
    cred_helper_out "$root" nocap 'echo username=alice; echo password=s3cret; echo state[]=ignored; echo continue=1'
    v config --global credential.helper "$root/both.sh"
    v clone -q http://127.0.0.1:$port/o.git c_both; cred_dump "$root"
    v config --global credential.helper "$root/rejected.sh"
    v clone -q http://127.0.0.1:$port/o.git c_rej; cred_dump "$root"
    v config --global credential.helper "$root/stateonly.sh"
    v clone -q http://127.0.0.1:$port2/o.git c_stateonly; cred_dump "$root"
    rm -f "$root/req2.log"
    v config --global credential.helper "$root/nocap.sh"
    v clone -q http://127.0.0.1:$port2/o.git c_nocap; cred_dump "$root"
    rm -f "$root/req2.log"
    kill $server $server2 2>/dev/null; wait $server $server2 2>/dev/null
}

s_basic_commit() {
    v init -q repo; cd repo
    echo hello >a.txt; mkdir -p dir/sub; echo deep >dir/sub/b.txt; echo x >dir/c
    chmod +x dir/c
    v add a.txt dir
    v commit -m "first commit"
    tick; echo more >>a.txt; v commit -a -m "second"
    v status
    v rev-parse HEAD
}

s_branch_ops() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v branch feat
    v branch feat
    v branch
    v branch -v
    v branch --show-current
    v branch -m feat feature
    v branch -c feature copy
    v branch
    v branch -d copy
    v branch -d main
    v branch -D nope
    v branch 'bad..name'
    v branch --list 'f*'
    # a branch whose tip object is missing: listing must carry on, not abort partway
    echo 1111111111111111111111111111111111111111 >.git/refs/heads/ghost-with-a-long-name
    v branch
    v branch -v
    v branch -v --merged
    # ranges naming nothing: the error quotes the whole argument, as git does
    v log '...x'
    v log 'x...'
    v log 'HEAD...x'
    v log 'x..'
    v log '..x'
    v log x
    v log '^x'
}

s_switch_checkout() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v switch -c feat
    tick; echo b >b; mkdir d; echo c >d/c; v add b d; v commit -q -m two
    v switch main
    ls
    v switch feat
    v switch feat
    v checkout main
    v checkout HEAD~0
    v switch -
    v checkout -b other main
    v switch --detach feat
    v switch main
    v checkout feat~1
    v checkout main
    echo dirty >a
    v switch feat
    v checkout -- a
    echo dirty >a
    v checkout a
    cat a
    echo untracked >b
    v switch feat
    rm b
    v switch nope
    v checkout -f feat
}

s_merge_ff_and_true() {
    v init -q repo; cd repo
    printf '1\n2\n3\n4\n5\n' >f; v add f; v commit -q -m base
    v branch topic
    tick; echo new >g; v add g; v commit -q -m "main adds g"
    v switch -q topic
    tick; printf '1\n2\n3\n4\n5\n6\n' >f; v commit -q -a -m "topic edits f"
    v switch -q main
    v merge topic
    v log --oneline
    v merge topic
    v branch ff main~1
    v switch -q ff
    v merge main
}

s_merge_conflict() {
    v init -q repo; cd repo
    printf '1\n2\n3\n' >f; v add f; v commit -q -m base
    v branch topic
    tick; printf '1\nmain\n3\n' >f; v commit -q -a -m "main"
    v switch -q topic
    tick; printf '1\ntopic\n3\n' >f; v commit -q -a -m "topic"
    v switch -q main
    v merge topic
    cat f
    v status
    v merge --abort
    v merge topic
    printf '1\nboth\n3\n' >f
    v add f
    tick; v commit -m "resolved"
    v merge --ff-only topic
}

s_tags() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v tag v1
    v tag -a v2 -m "release two"
    v tag
    v tag v1
    v tag -l 'v*'
    v tag -n
    v tag -d v1
    v tag -f v2 HEAD
    v show v2
}

s_reset() {
    v init -q repo; cd repo
    echo 1 >a; v add a; v commit -q -m one
    tick; echo 2 >a; echo b >b; v add a b; v commit -q -m two
    tick; echo 3 >a; v commit -q -a -m three
    v reset --soft HEAD~1
    v status --short
    v reset HEAD~1
    v status --short
    v reset --hard
    v status --short
    v reset --hard ORIG_HEAD
    v log --oneline
    echo junk >a; echo new >n; v add n
    v reset --hard HEAD~2
    ls
    v reset -- a
    v reset nope
}

s_restore() {
    v init -q repo; cd repo
    echo 1 >a; mkdir d; echo x >d/x; v add a d; v commit -q -m one
    echo 2 >a; v add a; echo 3 >a; rm d/x
    v restore a
    cat a
    v restore --staged a
    v status --short
    v restore d
    v restore --source=HEAD --staged --worktree a
    v restore nothere
    cat a
}

s_log_show_diff() {
    v init -q repo; cd repo
    printf 'l1\nl2\nl3\n' >f; echo x >g; v add f g; v commit -q -m "first" -m "body line"
    tick; printf 'l1\nchanged\nl3\nl4\n' >f; rm g; echo n >h; v add -A; v commit -q -m "second"
    tick; echo y >>h; v commit -q -a -m "third"
    v log
    v log --oneline
    v log -n 1 --stat
    v show
    v show HEAD~1
    v diff HEAD~2 HEAD
    v diff --stat HEAD~2 HEAD
    v diff --name-only HEAD~2
    v diff --name-status HEAD~2
    echo wt >>f
    v diff
    v add f
    v diff --cached
    v diff --exit-code --cached
    v diff --quiet
}

s_config() {
    v init -q repo; cd repo
    v config user.name "Some One"
    v config user.name
    v config --get user.name
    v config core.bare
    v config --type bool core.bare
    v config nope.key
    v config -l
    v config --add multi.key one
    v config --add multi.key two
    v config --get-regexp multi
    v config --unset user.name
    v config --remove-section multi
    v config --global alias.st status
    v config --global alias.st
    cat .git/config
}

s_check_ignore() {
    v init -q repo; cd repo
    printf '*.log\n!keep.log\nbuild/\n' >.gitignore
    mkdir build; touch a.log keep.log build/x other
    v check-ignore a.log
    v check-ignore -v a.log build/x keep.log other
    v check-ignore other
    v check-ignore -n -v other
}

s_check_attr() {
    g init -q repo; cd repo
    mkdir -p sub/deep
    printf '* zeta alpha=1\n*.c eol=lf text\n*.bin binary\n[attr]mac ma mb=1\nm.h mac\ndir/ dirattr\n"q r" sp\n' >.gitattributes
    printf '*.c beta -zeta\n*.h gamma\n*.txt -eol !alpha\n' >sub/.gitattributes
    printf '* nu\n' >sub/deep/.gitattributes
    printf '*.c omega v=info\n' >.git/info/attributes
    printf '* glob\n' >../global-attrs
    printf '[core]\n\tattributesFile = %s\n' "$PWD/../global-attrs" >>.git/config
    v check-attr -a x.c
    v check-attr -a sub/x.c sub/x.h x.c
    v check-attr -a x.c sub/x.h
    v check-attr --all sub/deep/f
    v check-attr -a x.bin m.h
    v check-attr -a sub/a.txt
    v check-attr text eol nope -- x.c sub/a.txt
    v check-attr text -- x.c
    v check-attr v eol -- x.c sub/x.c
    v check-attr -a dir/ dir sub/ sub
    v check-attr dirattr -- dir/ dir
    v check-attr -a "q r"
    v check-attr -a -- x.c
    v check-attr binary diff merge text -- x.bin
    v check-attr -z text -- x.c
    printf 'x.c\nsub/x.h\n' >in.txt
    STDIN_FILE=in.txt v check-attr --stdin -a
    STDIN_FILE=in.txt v check-attr --stdin text eol
    STDIN_FILE=in.txt v check-attr --stdin text --
    printf 'x.c\0sub/x.h\0' >in0.txt
    STDIN_FILE=in0.txt v check-attr --stdin -z -a
    STDIN_FILE=in0.txt v check-attr --stdin -z text
    (cd sub && v check-attr -a x.c ../x.c ./x.c)
    v check-attr text -- ../outside
    v check-attr
    v check-attr text
    v check-attr -- x.c
    v check-attr -a text -- x.c
    v check-attr -a --bogus x.c
    STDIN_FILE=in.txt v check-attr --stdin text -- x.c
}

s_fsck_verify_pack() {
    g init -q repo; cd repo
    seq 1 400 >big; g add big; g commit -q -m one
    tick; seq 1 400 | sed 's/^200$/changed/' >big; echo extra >small; g add .; g commit -qm two
    tick; seq 1 400 | sed 's/^100$/other/;s/^200$/changed/' >big; g commit -qam three
    g repack -adq --depth=10 --window=10
    v fsck
    v fsck --strict
    v verify-pack .git/objects/pack/*.idx
    v verify-pack --verbose .git/objects/pack/*.idx
    v verify-pack -s .git/objects/pack/*.idx
    echo orphan | g hash-object -w --stdin >/dev/null
    g commit-tree -m orphan HEAD^{tree} >/dev/null
    v fsck
    v fsck --unreachable --no-reflogs
    v fsck --root
    v fsck --no-dangling
}

s_fsck_missing() {
    g init -q repo; cd repo
    echo a >a; mkdir d; echo b >d/b; g add .; g commit -qm one
    blob=$(g rev-parse HEAD:d/b); rm .git/objects/${blob:0:2}/${blob:2}
    v fsck
}

s_cross_read() {
    g init -q repo; cd repo
    echo a >a; mkdir d; echo b >d/b; g add .; g commit -q -m one
    g branch side; tick; echo c >>a; g commit -qam two; g tag -a t1 -m t1
    g pack-refs --all
    v log --oneline --all
    v branch -a
    v status
    v show t1
    v cat-file -p HEAD
    v ls-tree -r HEAD
}

s_remote_fetch_push() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null; (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    v init -q repo; cd repo
    v remote add origin ../origin.git
    v remote
    v remote -v
    v fetch origin
    v branch -r
    v switch main
    tick; echo b >b; v add b; v commit -q -m two
    v push origin main
    v push origin main
    v push -u origin main:other
    v remote rename origin up
    v remote
    v remote remove up
    v remote
    # a remote section with no url: git lists the name and a tab, with no (fetch) label
    g config remote.nourl.fetch '+refs/heads/*:refs/remotes/nourl/*'
    v remote -v
    g config remote.nourl.pushurl ../origin.git
    v remote -v
}

s_pull() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    v clone -q origin.git repo
    (cd seed && tick && echo b >b && g add b && g commit -qm two && g push -q origin HEAD:main 2>/dev/null)
    cd repo
    v pull
    v pull
    v log --oneline
}

s_clone() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && mkdir d && echo c >d/c && g add . && g commit -qm one && g tag v1 && g branch side && g push -q --all origin 2>/dev/null && g push -q --tags origin 2>/dev/null)
    v clone origin.git repo
    v clone origin.git repo
    v clone --bare origin.git bare.git
    v clone -b side origin.git sidecl
    (cd repo && v status && v branch -a)
}

s_push_cases() {
    g init -q --bare origin.git
    v init -q repo; cd repo
    v remote add origin file://$ROOT/origin.git
    echo a >a; v add a; v commit -q -m one
    v push origin main
    v push -u origin main
    v tag v1; v tag -a v2 -m two
    v push origin --tags
    v push origin v1
    tick; echo b >b; v add b; v commit -q -m two
    v push
    v push origin main:feature
    v push origin :feature
    v push origin --delete v1
    v branch topic; v push origin topic
    # a second clone moves main so the next push is rejected
    g clone -q ../origin.git ../other 2>/dev/null
    (cd ../other && tick && echo x >x && g add x && g commit -qm other && g push -q origin HEAD:main 2>/dev/null)
    tick; echo c >c; v add c; v commit -q -m three
    v push
    v push origin +main
    v fetch origin
    v status
    v push --dry-run origin main
    # a missing source is a per-ref error (exit 1, nothing pushed); a malformed refspec is fatal (128)
    v push origin nope
    v push origin nope:refs/heads/x
    v push origin main:refs/heads/ok nope
    v push origin 'a b'
    v push origin 'main:'
    v push origin ''
    (cd ../origin.git && v for-each-ref)
}

s_pull_merge() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && printf '1\n2\n3\n' >f && g add f && g commit -qm base && g push -q origin HEAD:main 2>/dev/null)
    v init -q repo
    (cd repo && v remote add origin ../origin.git && v fetch -q origin && v switch -q main)
    (cd seed && tick && echo remote >r && g add r && g commit -qm remote-side && g push -q origin HEAD:main 2>/dev/null)
    cd repo
    tick; echo local >l; v add l; v commit -q -m local-side
    v pull --no-rebase
    v log --oneline
    v status
    v pull
}

s_fetch_prune_tags() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g tag v1 && g branch gone && g push -q origin --all 2>/dev/null && g push -q origin --tags 2>/dev/null)
    v clone -q origin.git repo
    (cd seed && g push -q origin :gone 2>/dev/null && tick && echo b >b && g add b && g commit -qm two && g tag v2 && g push -q origin HEAD:main --tags 2>/dev/null)
    cd repo
    v branch -a
    v fetch --prune
    v branch -a
    v tag
    v fetch --no-tags origin
    v fetch origin main
    cat .git/FETCH_HEAD
}

s_packed_refs() {
    g init -q repo; cd repo
    echo a >a; g add a; g commit -qm one; g branch keep; g branch gone; g branch mv
    tick; echo b >b; g add b; g commit -qm two; g tag t1; g tag -a t2 -m two
    g pack-refs --all
    v branch
    v branch -m mv moved
    v branch -d gone
    v tag -d t1
    v tag
    v tag -l
    v log --oneline keep..main
    v switch keep
    tick; echo c >c; v add c; v commit -q -m three
    v branch -f gone keep
    v branch -a
    v rev-parse t2 'main^{commit}' t2^{}
}

s_packed_refs_stale() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g branch gone1 && g branch gone2 && g branch gone3 && g branch keep && g push -q origin --all 2>/dev/null)
    g clone -q origin.git repo 2>/dev/null
    (cd repo && g pack-refs --all)
    (cd seed && g push -q origin :gone1 :gone2 :gone3 2>/dev/null)
    cd repo
    refs_state() { { echo "## packed"; cat .git/packed-refs; echo "## loose"; find .git/refs -type f | sort; } >>"$TRANSCRIPT"; }
    refs_state
    v fetch --prune
    refs_state
    v branch -a
    v for-each-ref
    v fsck
    g branch b1 keep; g branch b2 keep; g branch b3 keep
    (g pack-refs --all)
    v branch -D b1 b2 b3
    refs_state
    v branch -a
}

s_status_variants() {
    v init -q repo; cd repo
    printf '*.log\nbuild/\n' >.gitignore
    echo a >a; echo b >b; mkdir -p d/e; echo c >d/e/c; echo x >x.log; mkdir build; echo y >build/y
    ln -s a link; printf '\0bin' >bin.dat; echo '#!/bin/sh' >run.sh; chmod +x run.sh
    v status
    v status --short
    v status -sb
    v status --porcelain --ignored
    v add -A
    v status -s
    v commit -q -m one
    echo more >>a; rm b; chmod +x d/e/c; echo new >n; rm link; ln -s b link
    v status
    v status -s
    v status --porcelain -uno
    v diff
    v diff --stat
    v add -u
    v status -s
    v rm --cached a
    v rm -f n 2>&1
    v rm d/e/c
    v status -s
    v add nonexistent
    v add -n .
    v add -f x.log
    v status -s
    v commit -q -m two
    v diff --cached --exit-code
    v ls-files -s
    v ls-files -o --exclude-standard 2>/dev/null
}

s_rm_whole_tree() {
    v init -q repo; cd repo
    mkdir -p a/b c
    echo one >a/one; echo two >a/b/two; echo three >c/three; echo top >top
    v add -A; v commit -q -m one
    v rm -r .
    v status -s
    v ls-files
    # removing a single file takes the directories it empties with it
    mkdir -p d/e/f; echo deep >d/e/f/g
    v add -A; v commit -q -m two
    v rm d/e/f/g
    v status -s
    # a directory argument, recursively
    mkdir -p h/i; echo hi >h/i/j; echo keep >keep
    v add -A; v commit -q -m three
    v rm -r h
    v status -s
    v ls-files
    # a pathspec-limited spelling of the same thing
    mkdir -p k; echo kv >k/v
    v add -A; v commit -q -m four
    v rm -r -- k
    v status -s
}

s_plumbing() {
    v init -q repo; cd repo
    printf 'hello\n' >f; mkdir d; echo i >d/i
    v hash-object f
    v hash-object -w f
    v hash-object -t blob --stdin <f
    v add f d
    v write-tree
    v commit -q -m one
    v cat-file -t HEAD
    v cat-file -s HEAD
    v cat-file -p HEAD
    v cat-file -p 'HEAD^{tree}'
    v cat-file -e HEAD
    v cat-file -e 0000000000000000000000000000000000000001
    v ls-tree HEAD
    v ls-tree -r HEAD
    v ls-tree -r -l HEAD
    v ls-tree --name-only HEAD -- d
    v ls-files
    v ls-files -s
    v ls-files -c d
    v rev-parse HEAD HEAD^{tree} HEAD:f main
    v rev-parse --short=7 HEAD
    v rev-parse nonexistent
}

s_sha256() {
    v init -q --object-format sha256 repo 2>/dev/null || v init -q --object-format=sha256 repo
    cd repo
    echo hi >a; mkdir d; echo there >d/b
    v add a d
    v commit -q -m "sha256 commit"
    tick; echo more >>a; v commit -q -a -m two
    v log --oneline
    v cat-file -p HEAD
    v ls-tree -r HEAD
    v branch side; v switch -q side; tick; echo s >s; v add s; v commit -q -m side
    v switch -q main; v merge side
    v fsck
    v diff HEAD~1 HEAD --stat
}

s_odd_names() {
    v init -q repo; cd repo
    echo 1 >"with space"; echo 2 >"tab	name"; echo 3 >"uni-é-名前"; mkdir "dir with space"; echo 4 >"dir with space/f g"
    echo 5 >'-dash'; echo 6 >'quote"d'
    v add -A
    v status -s
    v ls-files
    v commit -q -m names
    v ls-tree -r HEAD
    v diff --name-only HEAD~0 2>/dev/null
    echo changed >>"with space"
    v diff
    v status -s
    v log --stat --oneline 2>/dev/null | head -20
}

s_merge_edge() {
    v init -q repo; cd repo
    printf 'a\nb\nc\n' >both; echo keep >del; echo m >mod; mkdir dirpath; echo f >dirpath/x
    v add -A; v commit -q -m base; v branch other
    tick; v rm -q del; echo changed >mod; echo mainadd >add; printf 'a\nB\nc\n' >both; v add -A; v commit -q -m main
    v switch -q other
    tick; echo modified >del; echo otherchange >mod; echo otheradd >add; printf 'a\nb\nC\n' >both; v add -A; v commit -q -m other
    v switch -q main
    v merge other
    v status -s
    v ls-files -u
    cat both; cat mod; cat add; cat del
    v merge --abort
    v status -s
    v merge --no-ff -m "custom message" other
    v log --oneline -n 2
}

s_corrupt() {
    g init -q repo; cd repo
    echo a >a; g add a; g commit -qm one; echo b >b; g add b; g commit -qm two
    blob=$(g rev-parse HEAD:a)
    chmod u+w .git/objects/${blob:0:2}/${blob:2}
    echo garbage >.git/objects/${blob:0:2}/${blob:2}
    vq fsck
    vq cat-file -p HEAD
    vq cat-file -p $blob
    vq status
    g gc -q 2>/dev/null; g repack -adq 2>/dev/null
}

s_corrupt_pack() {
    g init -q repo; cd repo
    seq 1 300 >big; g add big; g commit -qm one; seq 1 300 | sed 's/^5$/x/' >big; g commit -qam two
    g repack -adq
    pack=$(ls .git/objects/pack/*.pack)
    vq verify-pack ${pack%.pack}.idx
    vq fsck
    # flip one byte in the middle of the pack
    printf '\377' | dd of=$pack bs=1 seek=40 conv=notrunc 2>/dev/null
    vq verify-pack ${pack%.pack}.idx
    vq fsck
}

s_ssh() {
    export GIT_SSH="$FAKESSH"
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g tag v1 && g push -q origin HEAD:main --tags 2>/dev/null)
    v clone -q ssh://localhost$ROOT/origin.git repo
    (cd seed && tick && echo b >b && g add b && g commit -qm two && g push -q origin HEAD:main 2>/dev/null)
    cd repo
    v fetch
    v pull
    v log --oneline
    tick; echo c >c; v add c; v commit -q -m three
    v push
    v push origin HEAD:refs/heads/side
    v status
}

s_http() {
    local port=18771 root=$PWD
    mkdir -p http/cgi-bin http/repos
    printf '#!/bin/sh\nexport GIT_PROJECT_ROOT=%s/http/repos GIT_HTTP_EXPORT_ALL=1\nexec git http-backend\n' "$root" >http/cgi-bin/git-http
    chmod +x http/cgi-bin/git-http
    g init -q --bare http/repos/origin.git; g -C http/repos/origin.git config http.receivepack true
    (cd http && exec python3 -m http.server $port --cgi --bind 127.0.0.1 >/dev/null 2>&1) &
    local server=$!
    sleep 1
    local url=http://127.0.0.1:$port/cgi-bin/git-http/origin.git
    g clone -q http/repos/origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g tag v1 && g push -q origin HEAD:main --tags 2>/dev/null)
    v clone -q $url repo
    (cd seed && tick && echo b >b && g add b && g commit -qm two && g push -q origin HEAD:main 2>/dev/null)
    cd repo
    v fetch
    v pull
    v log --oneline
    tick; echo c >c; v add c; v commit -q -m three
    v push
    v push origin HEAD:refs/heads/side
    v status
    kill $server 2>/dev/null; wait $server 2>/dev/null
}

# Seeded random walk over the porcelain: the same operations, in the same order, for both tools.
random_walk() {
    local seed=$1 steps=${2:-60}
    RANDOM=$seed
    v init -q repo; cd repo
    local files=(a.txt b.txt c.txt d/e.txt d/f.txt g.txt)
    local branches=(main)
    local n i f b
    echo base >a.txt; v add a.txt; v commit -q -m base
    for ((n = 0; n < steps; n++)); do
        case $((RANDOM % 14)) in
            0|1|2) # edit a file with a few short lines (small alphabet => frequent merge overlaps)
                f=${files[$((RANDOM % ${#files[@]}))]}; mkdir -p "$(dirname "$f")"
                : >"$f"; for ((i = 0; i < RANDOM % 5 + 1; i++)); do echo "line $((RANDOM % 4))" >>"$f"; done ;;
            3) f=${files[$((RANDOM % ${#files[@]}))]}; rm -f "$f"; v rm -q --cached "$f" 2>/dev/null ;;
            4|5) v add -A ;;
            6|7) tick; v commit -q -a -m "step $n"; v commit -q -m "step $n staged" ;;
            8) b="br$((RANDOM % 3))"; v branch "$b" 2>/dev/null; branches+=("$b") ;;
            9) b=${branches[$((RANDOM % ${#branches[@]}))]}; v switch -q "$b" ;;
            10) b=${branches[$((RANDOM % ${#branches[@]}))]}; tick; v merge -q -m "merge $b" "$b" ;;
            11) case $((RANDOM % 3)) in 0) v reset -q --soft HEAD~1 ;; 1) v reset -q --mixed HEAD~1 ;; 2) v reset -q --hard HEAD~1 ;; esac ;;
            12) f=${files[$((RANDOM % ${#files[@]}))]}; v restore "$f"; v restore --staged "$f" ;;
            13) v status -s; v diff --stat; v log --oneline -n 3 ;;
        esac
        # leave any conflict behind so the walk can continue
        if v ls-files -u | grep -q .; then v merge --abort; fi
    done
    v status -s; v log --oneline --all
}
s_random_1() { random_walk 1; }
s_random_2() { random_walk 2; }
s_random_3() { random_walk 3; }
s_random_4() { random_walk 4; }
s_random_5() { random_walk 5; }
s_random_6() { random_walk 6; }
s_random_7() { random_walk 7; }
s_random_8() { random_walk 8; }

s_renames() {
    v init -q repo; cd repo
    seq 1 40 >a.txt; seq 100 140 >b.txt; mkdir -p src/old; seq 1 30 >src/old/deep.txt; echo tiny >t.txt; seq 1 20 >c.txt
    v add -A; v commit -q -m base
    mv a.txt renamed.txt                       # exact
    seq 100 140 | sed 's/^120$/CHANGED/' >bb.txt; rm b.txt     # similar
    mkdir -p src/new; mv src/old/deep.txt src/new/deep.txt      # directory move
    chmod +x c.txt; mv c.txt c2.txt                              # rename + mode
    echo unrelated >u.txt; rm t.txt                              # not a rename
    v status
    v status -s
    v add -A
    v status
    v status -s
    v diff --cached
    v diff --cached --stat
    v diff --cached --name-status
    v diff --cached --name-only
    tick; v commit -m "renames"
    v show
    v log --stat --oneline
    v diff HEAD~1 HEAD --name-status
    v diff HEAD~1 HEAD
}

s_restore_edges() {
    v init -q repo; cd repo
    echo 1 >a; echo 2 >b; mkdir d; echo 3 >d/x; v add -A; v commit -q -m one
    echo new >n; v add n                       # staged, absent from HEAD
    echo wt >>a
    v restore n
    v restore --staged n
    v status -s
    v restore --staged n
    v restore --worktree n
    v restore missing
    v restore --staged missing
    v restore --source=HEAD n
    v restore --source=HEAD --worktree b
    v restore --source=HEAD~0 --staged d
    v restore a b
    v restore a nope
    cat a
    rm b; v restore --staged b
    v restore b
    v status -s
    v restore .
    v restore --staged .
    v status -s
    v restore --source=nonexistent a
}

s_rename_config() {
    v init -q repo; cd repo
    seq 1 30 >a; v add a; v commit -q -m one
    mv a b; v add -A
    v status -s
    v diff --cached --name-status
    v config diff.renames false
    v status -s
    v diff --cached --name-status
    v config diff.renames true
    v config status.renames false
    v status -s
    v diff --cached --name-status
    tick; v commit -q -m two
    v log --name-status --oneline -n 1
    v config diff.renames copies
    v log --name-status --oneline -n 1
}

merge_rename_case() {
    local name=$1
    v init -q "$name"; cd "$name"
    seq 1 40 >f; seq 100 130 >other; echo keep >k; v add -A; v commit -q -m base; v branch side
    case $name in
    modify_vs_rename)
        mv f g; v add -A; tick; v commit -q -m "main renames"
        v switch -q side; seq 1 40 | sed 's/^20$/twenty/' >f; tick; v commit -q -a -m "side edits"; v switch -q main ;;
    delete_vs_rename)
        mv f g; v add -A; tick; v commit -q -m "main renames"
        v switch -q side; v rm -q f; tick; v commit -q -m "side deletes"; v switch -q main ;;
    same_rename)
        mv f g; v add -A; tick; v commit -q -m "main renames"
        v switch -q side; mv f g; echo extra >>g; v add -A; tick; v commit -q -m "side renames too"; v switch -q main ;;
    split_rename)
        mv f g; v add -A; tick; v commit -q -m "main renames"
        v switch -q side; mv f h; v add -A; tick; v commit -q -m "side renames elsewhere"; v switch -q main ;;
    rename_over_add)
        mv f g; v add -A; tick; v commit -q -m "main renames"
        v switch -q side; echo different >g; v add -A; tick; v commit -q -m "side adds g"; v switch -q main ;;
    theirs_renames)
        echo mainedit >>f; v add -A; tick; v commit -q -m "main edits"
        v switch -q side; mv f g; v add -A; tick; v commit -q -m "side renames"; v switch -q main ;;
    esac
    v merge side
    v status -s
    v ls-files -s
    v log --oneline --all
    cat * 2>/dev/null | head -50
}
s_merge_rename_modify() { merge_rename_case modify_vs_rename; }
s_merge_rename_delete() { merge_rename_case delete_vs_rename; }
s_merge_rename_same() { merge_rename_case same_rename; }
s_merge_rename_split() { merge_rename_case split_rename; }
s_merge_rename_over_add() { merge_rename_case rename_over_add; }
s_merge_rename_theirs() { merge_rename_case theirs_renames; }

s_log_paths() {
    v init -q repo; cd repo
    mkdir -p src docs; echo 1 >src/a; echo 1 >docs/d; echo 1 >top
    v add -A; tick; v commit -q -m "c1 base"
    v branch feat
    echo 2 >>src/a; v add -A; tick; v commit -q -m "c2 src a"
    echo 2 >>docs/d; v add -A; tick; v commit -q -m "c3 docs"
    v switch -q feat
    echo 2 >>top; v add -A; tick; v commit -q -m "f1 top"
    echo 3 >>src/a; v add -A; tick; v commit -q -m "f2 src a (feat)"
    echo x >src/b; v add -A; tick; v commit -q -m "f3 src b"
    v switch -q main
    tick; v merge -q -m "merge feat" feat 2>/dev/null; v status -s
    echo 4 >>docs/d; v add -A; tick; v commit -q -m "c4 docs"
    v log --oneline
    v log --oneline -- src
    v log --oneline -- src/a
    v log --oneline -- docs top
    v log --oneline -- nothing
    v log --oneline --first-parent
    v log --oneline --reverse
    v log --oneline --topo-order
    v log --oneline --date-order
    v log --oneline -n 2 --skip 1
    v log --oneline feat..main
    v log --oneline main..feat
    v log --oneline main...feat
    v log --oneline --all
    v log --oneline HEAD~2
    v log --oneline --since=@1700000100
    v log --oneline --until=@1700000100
    v log -p -- src/b
    v log --stat -- docs
    v log --name-status --oneline -- src
    v show HEAD~1 -- docs
    v log --oneline -n 1 -- src/a
}

s_cat_file_batch() {
    v init -q repo; cd repo
    printf 'hello\n' >f; mkdir d; printf 'inner\n' >d/i; seq 1 800 >big
    v add -A; v commit -q -m one; v tag -a t -m tagmsg
    local c t b
    c=$(g rev-parse HEAD); t=$(g rev-parse 'HEAD^{tree}'); b=$(g rev-parse HEAD:f)
    vin "$c
$t
$b
HEAD
HEAD:f
HEAD:d/i
nonexistent
t
${c:0:8}
" cat-file --batch
    vin "$c
$t
$b
HEAD:d
missing
" cat-file --batch-check
    vin "$b
HEAD
" cat-file --batch-check='%(objectname) %(objecttype) %(objectsize)'
    vin "$b
" cat-file --batch='%(objecttype) %(objectsize) %(rest)'
    vs cat-file --batch-all-objects --batch-check
    v cat-file -p t
    v cat-file -t t
    v cat-file -s HEAD:big
}

s_pack_deltas() {
    export GIT_SSH="$FAKESSH"
    g init -q origin; cd origin
    seq 1 2000 >big; echo a >small; mkdir d; seq 1 300 >d/other
    g add -A; g commit -qm c0
    for i in 1 2 3 4 5 6 7 8; do
        tick; seq 1 2000 | sed "s/^$((i * 100))$/edited $i/" >big; echo "$i" >>small; seq 1 300 | sed "s/^$((i * 10))$/e$i/" >d/other
        g add -A; g commit -qm "c$i"
    done
    g tag -a v1 -m annotated
    g repack -adfq --depth=20 --window=50
    cd ..
    v clone -q ssh://localhost$ROOT/origin cl
    v clone -q origin cl2
    cd origin
    vq verify-pack --verbose .git/objects/pack/*.idx
    vq fsck
    vs cat-file --batch-all-objects --batch
    v log --oneline -p -- big
    v log --stat --oneline
    v diff v1~7 v1 --stat
    v show v1
    cd ../cl
    vq fsck
    v log --oneline -n 3
    cd ../cl2
    vq fsck
    v status
    cd ../origin
    for i in 11 12 13; do tick; seq 1 2000 | sed "s/^$((i * 50))$/late $i/" >big; g add -A; g commit -qm "late$i"; done
    cd ../cl
    v fetch
    v pull
    vq fsck
    v log --oneline -n 4
    v diff HEAD~3 HEAD --stat
    cd ../cl2
    v fetch
    v pull
    vq fsck
}

s_histogram() {
    RANDOM=42
    v init -q repo; cd repo
    local round i lines alphabet
    for round in 1 2 3 4 5 6 7 8 9 10 11 12; do
        alphabet=$((RANDOM % 6 + 2)); lines=$((RANDOM % 40 + 5))
        : >f; for ((i = 0; i < lines; i++)); do echo "l$((RANDOM % alphabet))" >>f; done
        v add f; tick; v commit -q -m "a$round"
        : >f; for ((i = 0; i < lines + RANDOM % 7 - 3; i++)); do echo "l$((RANDOM % alphabet))" >>f; done
        v add f; tick; v commit -q -m "b$round"
        v diff HEAD~1 HEAD --diff-algorithm=histogram
        v diff HEAD~1 HEAD --diff-algorithm=histogram --stat
    done
}

s_log_forms() {
    v init -q repo; cd repo
    printf 'one\n' >a; mkdir d; echo x >d/b; v add a d; v commit -q -m "first subject" -m "first body line"
    tick; echo two >>a; v commit -q -a -m "second"
    v branch topic; v tag light; v tag -a ann -m annotated
    tick; echo three >>a; echo y >d/c; v add d/c; v commit -q -a -m "third subject
continued line" -m "body para"
    v switch -q topic
    tick; echo t >t; v add t; v commit -q -m "topic work"
    v switch -q main
    tick; v merge -q --no-ff topic -m "merge topic"
    v log -1
    v log -n1
    v log -2 --oneline
    v log -3
    v log --oneline -n 2
    v log --format=%H
    v log --format='%h|%T|%s|%an|%ae|%P'
    v log --format='%H %h %t %p%n%b%%'
    v log --format='%ad|%aD|%ai|%aI|%as|%at|%cd|%cn|%ce|%ct'
    v log --format='%D|%d' --all
    v log --date=iso --format='%ad %cd'
    v log --date=short --format=%ad
    v log --date=relative --format=%ar
    v log --date=relative -2
    v log --date=iso -1
    v log --date=raw --format=%ad
    v log --date=rfc2822 --format=%ad
    v log --date=iso-strict --format=%ad
    v log --pretty=oneline
    v log --pretty=short
    v log --pretty=medium
    v log --pretty=full
    v log --pretty=fuller
    v log --pretty=format:'%h %s'
    v log --pretty=tformat:'%h %s'
    v log --pretty='%h %s'
    v log --format='%h %s %Z %'
    v log --stat --format=%s
    v log --name-only --format=%s
    v log --name-status --pretty=oneline
    v log -1 -p --format='%h'
    v log --pretty=bogus
    v log --date=bogus
    v show --stat
    v show --name-only HEAD~1
    v show --name-status HEAD~1
    v show --format=%s -s
    v show --pretty=full --stat HEAD~2
    v show --date=iso --stat HEAD~2
}

s_verify_pack_v() {
    g init -q repo; cd repo
    seq 1 2000 >big; echo a >small; mkdir d; seq 1 300 >d/other
    g add -A; g commit -qm c0
    for i in 1 2 3 4 5 6; do
        tick; seq 1 2000 | sed "s/^$((i * 100))$/edited $i/" >big; echo "$i" >>small
        g add -A; g commit -qm "c$i"
    done
    g repack -adfq --depth=20 --window=50
    local idx; idx=$(ls .git/objects/pack/*.idx)
    v verify-pack -v "$idx"
    v verify-pack -s "$idx"
    v verify-pack "$idx"
}

seq_setup() {
    v init -q repo; cd repo
    printf '1\n2\n3\n4\n5\n' >f; echo base >g; v add f g; v commit -q -m base
    v branch topic
    tick; echo m >m; v add m; v commit -q -m "main adds m"
    v switch -q topic
    tick; echo b >b; v add b; v commit -q -m "topic adds b"
    tick; printf '1\n2\n3\n4\n5\nsix\n' >f; v commit -q -a -m "topic edits f" -m "with a body"
    tick; echo c >c; mkdir d; echo deep >d/e; v add c d; chmod +x c; v add c; v commit -q -m "topic adds c and d/e"
    tick; printf 'ONE\n2\n3\n4\n5\nsix\n' >f; v commit -q -a -m "topic edits line one"
    v switch -q main
}

s_cherry_pick() {
    seq_setup
    tick; v cherry-pick topic~3
    tick; v cherry-pick topic~2
    v log --format='%H %an <%ae> %ad|%cn %cd|%s|%b'
    v show --stat HEAD
    tick; v cherry-pick topic~1 topic
    v log --oneline
    v diff topic HEAD --stat
    v cherry-pick topic~9
    vq cherry-pick
    v cherry-pick --continue
    v cherry-pick --abort
}

s_cherry_pick_options() {
    seq_setup
    tick; v cherry-pick -n topic~2
    v status --short
    tick; v commit -q -m "manual commit"
    tick; v cherry-pick -n topic~3 topic~1
    v status --short
    tick; v commit -q -m "manual commit 2"
    v reset -q --hard main~2
    tick; v cherry-pick topic~3..topic
    v log --oneline
    echo dirty >>f
    vq cherry-pick topic~3
    v status --short
    tick; vq cherry-pick topic~9..topic~8
    v checkout -- f
    v switch -q --detach topic~3
    tick; v cherry-pick topic~2
    v log --oneline
}

s_cherry_pick_conflict() {
    seq_setup
    tick; printf 'main-one\n2\n3\n4\n5\n' >f; v commit -q -a -m "main edits f"
    tick; v cherry-pick topic
    v status
    cat f
    v cherry-pick --continue
    v cherry-pick topic~1
    v cherry-pick --abort
    v status --short
    cat f
    tick; v cherry-pick topic
    printf 'resolved\n2\n3\n4\n5\nsix\n' >f
    v add f
    tick; v cherry-pick --continue
    v log --format='%H|%an|%cn|%s|%P' -3
    v status
    v cherry-pick --continue
    v cherry-pick --abort
}

s_cherry_pick_conflict_nocommit() {
    seq_setup
    tick; printf 'main-one\n2\n3\n4\n5\n' >f; v commit -q -a -m "main edits f"
    v cherry-pick -n topic
    v status --short
    cat f
    v cherry-pick topic~1
}

s_cherry_pick_delete_conflict() {
    v init -q repo; cd repo
    echo a >a; echo b >b; v add a b; v commit -q -m base
    v branch topic
    tick; v rm -q b; v commit -q -m "main deletes b"
    v switch -q topic
    tick; echo bb >b; v commit -q -a -m "topic edits b"
    tick; echo aa >a; v commit -q -a -m "topic edits a"
    v switch -q main
    tick; v cherry-pick topic~1
    v status --short
    ls
    v cherry-pick --abort
    v status --short
    tick; v cherry-pick topic
    v status --short
}

s_cherry_pick_empty_and_merge() {
    seq_setup
    tick; v cherry-pick topic~3
    v branch side
    v switch -q side
    tick; echo s >s; v add s; v commit -q -m side
    v switch -q main
    tick; v merge -q --no-ff side -m "merge side"
    v cherry-pick HEAD
    v cherry-pick nonexistent
    v status --short
}

s_revert() {
    seq_setup
    tick; v cherry-pick topic~3 topic~2 topic~1
    v log --oneline
    tick; v revert --no-edit HEAD
    v log --format='%H|%an|%cn|%s|%b'
    tick; v revert --no-edit HEAD
    v log -1 --format=%B
    tick; v revert --no-edit HEAD~3
    v status
    v revert HEAD
    vq revert
    v revert --continue
    v revert --abort
}

s_revert_options() {
    seq_setup
    tick; v cherry-pick topic~3 topic~2 topic~1
    v revert -n --no-edit HEAD~1
    v status --short
    tick; v commit -q -m "manual"
    v revert --no-edit HEAD~1 HEAD
    v revert --no-edit main~100
    tick; v revert --no-edit HEAD~0
    v log --oneline
}

s_revert_conflict() {
    seq_setup
    tick; v cherry-pick topic~3 topic~2
    tick; printf '1\n2\n3\n4\nfive\nsix\n' >f; v commit -q -a -m "main edits f again"
    tick; v revert --no-edit HEAD~1
    v status
    cat f
    v revert --continue
    v revert --abort
    v status --short
    cat f
    tick; v revert --no-edit HEAD~1
    printf '1\n2\n3\n4\nfive\n' >f
    v add f
    tick; v revert --continue
    v log --format='%H|%an|%cn|%s|%b' -2
    v status --short
}

s_reflog() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    tick; echo b >>a; v commit -q -a -m two
    v branch topic
    v switch -q topic
    tick; echo c >>a; v commit -q -a -m "three on topic"
    v switch -q main
    tick; v merge -q --no-ff topic -m "merge topic"
    v reset -q --hard HEAD~1
    v switch -q --detach topic
    v switch -q main
    v reflog
    v reflog show
    v reflog show HEAD
    v reflog --oneline
    v reflog -n 2
    v reflog -2
    v reflog show -n1
    v reflog main
    v reflog show main -n 3
    v reflog refs/heads/main -n 2
    v reflog topic
    v reflog show topic --oneline
    v reflog nonexistent
    v branch empty
    v reflog empty
    v branch -m topic renamed
    v reflog renamed
    v reflog show HEAD -n 0
}

s_stash() {
    v init -q repo; cd repo
    printf '1\n2\n3\n' >a; echo b >b; mkdir d; echo c >d/c; v add a b d; v commit -q -m "first subject" -m "body"
    v stash list
    v stash
    v stash push
    v stash show
    v stash pop
    v stash apply
    v stash drop
    tick; echo a2 >>a; v stash
    v stash list
    v status --short
    v stash show
    v stash show -p
    v stash show --name-only
    v stash show --name-status
    v stash show stash@{0}
    v stash apply
    v status --short
    v stash list
    v checkout -q -- a
    tick; echo new >n; v add n; echo b2 >>b; v add b; echo b3 >>b; rm d/c
    v stash push -m "my message"
    v status --short
    ls
    v stash list
    tick; echo more >>a; v stash
    v stash list
    v stash show 1
    v stash show stash@{1} -p
    v stash pop
    v status --short
    v stash list
    v stash pop stash@{0}
    v status --short
    v stash list
    v stash pop
    v stash drop
    v stash show
    v stash apply
    v stash show stash@{3}
}

s_stash_stack() {
    v init -q repo; cd repo
    echo a >a; echo b >b; echo c >c; v add a b c; v commit -q -m base
    for i in 1 2 3 4; do
        tick; echo "change $i" >>a; v stash push -m "stash number $i"
    done
    tick; echo wip >b; v stash
    v stash list
    v stash drop stash@{2}
    v stash list
    v stash drop 0
    v stash list
    v stash pop stash@{1}
    v stash list
    v checkout -q -- .
    v stash drop stash@{9}
    v stash drop stash@{1}
    v stash list
    v stash drop
    v stash list
    v stash clear
    v stash list
    v stash drop
    v branch other
    tick; echo x >>a; v stash
    v switch -q other
    tick; echo y >>c; v commit -q -a -m "other edits c"
    v stash apply
    v stash list
    v log --oneline --format='%h %s %p' stash
    v switch -q --detach main
    tick; echo z >>b; v stash
    v stash list
    # -q silences the "Dropped ..." line for drop and for pop's drop; the stack must still shrink
    tick; echo q1 >>b; v stash -q
    v stash drop -q
    v stash list
    v stash drop -q stash@{0}
    v stash list
    tick; echo q2 >>b; v stash -q
    v stash pop -q
    v stash list
    v status --short
    # create / store / save. A clean tree: create prints nothing and fails nothing.
    v stash create
    echo "rc=$?"
    tick; echo cr >>a; echo ix >c; v add c; echo ut >untracked
    v stash create
    v stash create "two words"
    v stash create -x y
    v stash create ""
    v stash list
    v status --short
    # store: the commit create printed, with and without -m, twice (the same id is a no-op).
    tick; v stash create "to store"; sc=$(<"$TMPOUT")
    v stash store
    echo "rc=$?"
    v stash store one two
    echo "rc=$?"
    v stash store nonexistent
    echo "rc=$?"
    v stash store HEAD
    echo "rc=$?"
    v stash store -q "$sc"
    echo "rc=$?"
    v stash store -m ignored "$sc"
    echo "rc=$?"
    v stash list
    tick; echo cr2 >>a; v stash create "second"; sc=$(<"$TMPOUT")
    v stash store -m "stored by hand" "$sc"
    echo "rc=$?"
    v stash list
    v reflog show stash
    v status --short
    # save: push's old spelling; the message is the remaining words, which win over -m.
    v stash save
    echo "rc=$?"
    v stash list
    tick; echo s1 >>a; v stash save a b c
    echo "rc=$?"
    tick; echo s2 >>a; v stash save -m foo
    tick; echo s3 >>a; v stash save -m foo bar
    tick; echo s4 >>a; v stash save -q "quiet one"
    tick; echo s5 >>a; v stash save ""
    tick; echo s6 >>a; v stash save -u with untracked
    v stash save -k
    echo "rc=$?"
    v stash list
    v status --short
}

s_stash_conflict() {
    v init -q repo; cd repo
    printf '1\n2\n3\n' >f; echo g >g; v add f g; v commit -q -m base
    tick; printf '1\nstashed\n3\n' >f; echo gg >>g; v stash
    tick; printf '1\nupstream\n3\n' >f; v commit -q -a -m upstream
    v stash apply
    cat f
    v status --short
    v stash pop
    v stash list
    v reset -q --hard
    echo dirty >f
    v stash apply
    v status --short
    v checkout -q -- f
    v stash apply
    v stash list
    v stash pop
}

s_clean() {
    v init -q repo; cd repo
    mkdir tdir; echo t >tdir/t; echo k >keep; v add tdir keep; v commit -q -m x
    printf 'ign*\nidir/\n*.o\n' >.gitignore
    mkdir udir idir tdir/sub empty a a-b; echo u >udir/u; echo i >udir/ign1; echo x >idir/x
    echo o >tdir/a.o; echo u2 >tdir/u2; echo n >tdir/ign3; echo top >top; echo topo >top.o
    echo s >tdir/sub/s; echo z >a/z; echo ab >a-b/f; echo f >a.c; echo "sp ace" >"sp ace"
    mkdir -p deep/er/est; echo d >deep/er/est/f
    mkdir -p mixed/inner; echo m >mixed/inner/m; echo ig >mixed/ignored.o
    v clean
    v clean -d
    v clean -n
    v clean -n -d
    v clean -n -x
    v clean -n -d -x
    v clean -n -X
    v clean -n -d -X
    v clean -n -dx tdir
    v clean -n -d udir a
    v clean -n -d tdir/sub top
    cd tdir
    v clean -n
    v clean -n -d
    v clean -n -dx
    v clean -n -d ../a ../udir
    cd ..
    v clean -f -q
    ls
    v status --short
    v clean -fd
    ls
    v status --short --ignored
    v clean -fdx
    ls
    v status --short --ignored
    v clean -fdx
}

s_clean_edge() {
    v init -q repo; cd repo
    echo k >keep; v add keep; v commit -q -m x
    mkdir nested; git init -q nested; echo n >nested/n
    echo u >u
    v clean -n -d
    v clean -fd
    ls
    v clean -fdff -n
    git config clean.requireForce false
    v clean -n
    v clean
    ls
    git config --unset clean.requireForce
    echo again >again
    v clean -x -X
    v clean -d -f again
    ls
}

s_mv() {
    v init -q repo; cd repo
    echo a >a; echo b >b; mkdir d e; echo db >d/b; echo dc >d/c; mkdir d/sub; echo s >d/sub/s
    echo x >e/x; echo u >untracked; ln -s a link; chmod +x b
    v add a b d e link; v commit -q -m base
    v mv nosuch x
    v mv untracked x
    v mv a b
    v mv a nodir/a
    v mv a a2
    v status --short
    v mv a2 a
    v mv -n a z
    v mv -v a z
    v mv z e
    v status --short
    ls e
    v mv e/z .
    v mv a b e
    v mv -f a e/b
    v mv d d/sub
    v mv -n d f
    v mv -v d f
    v status --short
    ls f f/sub
    v mv f/sub top
    echo extra >f/extra
    v mv f g
    ls g
    v status --short
    v mv link link2
    v mv b link2 d
    v mv -k nosuch link2 e
    v mv -k nosuch link2
    vq mv
    vq mv a
    v mv x y z
    mkdir -p deep/dir; echo q >deep/dir/q; v add deep; v commit -q -m deep
    cd deep
    v mv dir/q ../qq
    v mv dir ../ddir
    cd ..
    v status --short
    v diff --stat HEAD
}

# Reachable objects, sorted: the content-level check that two repositories hold the same history.
objset() { g rev-list --objects --all | sort; }

s_count_objects() {
    v init -q repo; cd repo
    v count-objects
    v count-objects -v
    echo a >a; mkdir d; echo b >d/b; v add a d; v commit -q -m one
    tick; echo more >>a; v commit -q -a -m two
    v count-objects
    v count-objects -v
    v count-objects -vH
    v count-objects -H
    g repack -adq
    v count-objects -v
    echo loose >c; v add c; v commit -q -m three
    DROP_OUT='^size-pack' v count-objects -v
    g repack -q
    DROP_OUT='^size-pack' v count-objects -v
    g repack -adq
    echo junk >.git/objects/pack/garbage.tmp
    mkdir -p .git/objects/ab; echo junk >.git/objects/ab/notanobject
    DROP_OUT='^size-pack' v count-objects -v
    v count-objects
}

s_gc() {
    g config --global gc.cruftPacks false
    v init -q repo; cd repo
    echo a >a; mkdir d; echo b >d/b; v add a d; v commit -q -m one
    tick; echo more >>a; v commit -q -a -m two
    v branch topic; v tag light; v tag -a ann -m annotated
    v switch -q topic; tick; echo t >t; v add t; v commit -q -m topic; v switch -q main
    tick; echo stash >>a; v stash
    DROP_OUT='^size-pack' v count-objects -v
    v gc
    DROP_OUT='^size-pack' v count-objects -v
    objset
    v fsck
    v log --oneline --all
    g for-each-ref
    cat .git/packed-refs
    v gc
    DROP_OUT='^size-pack' v count-objects -v
    tick; echo three >>a; v commit -q -a -m three
    v branch -D topic
    tick; v gc --no-prune
    DROP_OUT='^size-pack' v count-objects -v
    objset
    v fsck
    g show-ref
    v status
    v log --oneline
    v checkout -q -- . 
    v reflog
    v cat-file -p ann
    v rev-parse ann^{}
}

s_gc_prune() {
    g config --global gc.cruftPacks false
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    tick; echo b >b; v add b; v commit -q -m two
    # unreachable objects: one old, one new, one old inside a pack, one new inside a pack
    echo old-loose | g hash-object -w --stdin >/dev/null
    echo new-loose | g hash-object -w --stdin >/dev/null
    old=$(echo old-loose | g hash-object --stdin)
    touch -d '30 days ago' ".git/objects/${old:0:2}/${old:2}"
    g repack -adq
    DROP_OUT='^size-pack' v count-objects -v
    echo packed-unreachable | g hash-object -w --stdin >/dev/null
    echo late-loose | g hash-object -w --stdin >/dev/null
    late=$(echo late-loose | g hash-object --stdin)
    touch -d '30 days ago' ".git/objects/${late:0:2}/${late:2}"
    v gc
    DROP_OUT='^size-pack' v count-objects -v
    g cat-file -e "$old" 2>&1; echo "old exists: $?"
    g cat-file -e "$late" 2>&1; echo "late exists: $?"
    v fsck
    objset
    v gc --prune=now
    DROP_OUT='^size-pack' v count-objects -v
    v fsck
    objset
    echo x | g hash-object -w --stdin >/dev/null
    v gc --prune=3.days.ago
    DROP_OUT='^size-pack' v count-objects -v
    v gc --prune=bogus
    v gc
    v gc --auto
}

s_gc_reflog() {
    g config --global gc.cruftPacks false
    local day=86400
    at() { export GIT_AUTHOR_DATE="$1 +0000" GIT_COMMITTER_DATE="$1 +0000"; }
    v init -q repo; cd repo
    at $((NOW0 - 100 * day)); echo 1 >f; v add f; v commit -q -m "100 days old"
    at $((NOW0 - 60 * day)); echo 2 >>f; v commit -q -a -m "60 days old"
    at $((NOW0 - 40 * day)); v switch -q -c tmp; echo 3 >>f; v commit -q -a -m "unreachable at 40 days"
    v switch -q main; v branch -D tmp
    at $((NOW0 - 10 * day)); echo 4 >>f; v commit -q -a -m "10 days old"
    at $((NOW0 - 5 * day)); echo 5 >>f; v commit -q -a -m "5 days old, undone"
    at $((NOW0 - 1 * day)); v reset -q --hard HEAD~1
    v reflog
    v gc
    v reflog
    v reflog main
    DROP_OUT='^size-pack' v count-objects -v
    objset
    v fsck
    v gc --prune=now
    DROP_OUT='^size-pack' v count-objects -v
    objset
    v fsck
    git config gc.reflogExpire never
    git config gc.reflogExpireUnreachable never
    at $((NOW0 - 1 * day)); echo 6 >>f; v commit -q -a -m six
    at $((NOW0 - 200 * day)); v branch old; v tag -a oldtag -m old
    v gc
    v reflog
    objset
}

# The cruft pack(s), as real git reads them: objects in pack order ("<id> <type> <size> [<depth>
# <base>]", no compressed sizes or offsets) and the age recorded for each id.
cruftshape() {
    echo "## cruftshape" >>"$TRANSCRIPT"
    local idx m n
    for idx in .git/objects/pack/*.idx; do
        [ -e "$idx" ] || continue
        m=${idx%.idx}.mtimes
        [ -f "$m" ] || continue
        echo "# cruft pack" >>"$TRANSCRIPT"
        g verify-pack -v "$idx" | awk 'NF == 5 && $1 ~ /^[0-9a-f]+$/ { print $1, $2, $3 } NF == 7 && $1 ~ /^[0-9a-f]+$/ { print $1, $2, $3, $6, $7 }' >>"$TRANSCRIPT"
        n=$(g show-index <"$idx" | wc -l)
        echo "mtimes header: $(od -An -tx1 -N12 "$m" | tr -d ' \n')" >>"$TRANSCRIPT"
        paste <(g show-index <"$idx" | awk '{print $2}' | sort) <(od -An -tu4 --endian=big -j12 -N$((n * 4)) -v "$m" | tr -s ' ' '\n' | grep .) >>"$TRANSCRIPT"
    done
}

# Every object git can find (reachable or not) and the loose files left behind.
allobjects() {
    echo "## all objects" >>"$TRANSCRIPT"
    g cat-file --batch-all-objects --batch-check | sort >>"$TRANSCRIPT"
    echo "## loose" >>"$TRANSCRIPT"
    (cd .git/objects && find . -type f -path './??/*' | sort) >>"$TRANSCRIPT"
    echo "## pack files" >>"$TRANSCRIPT"
    (cd .git/objects/pack 2>/dev/null && ls | sed 's/^pack-[0-9a-f]*//' | sort | uniq -c) >>"$TRANSCRIPT"
}

# Give every unreachable object (or the ones named) the modification time "$1".
age_unreachable() {
    local when=$1 oid
    for oid in $(g fsck --unreachable --no-reflogs --no-progress 2>/dev/null | awk '{print $3}'); do
        touch -d "$when" ".git/objects/${oid:0:2}/${oid:2}" 2>/dev/null
    done
}
age() { touch -d "$2" ".git/objects/${1:0:2}/${1:2}"; }

s_gc_cruft() {
    OLD='2026-09-01 00:00:00'; NEW='2026-10-20 00:00:00'; CUT=2026-10-05T00:00:00Z
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    tick; echo b >b; v add b; v commit -q -m two
    v switch -q -c topic; tick; echo t >t; v add t; v commit -q -m topic; v switch -q main
    v branch -D topic
    tick; echo stash-me >>a; v stash -q
    g stash drop -q
    echo loose-new | g hash-object -w --stdin >/dev/null
    echo loose-old | g hash-object -w --stdin >/dev/null
    age_unreachable "$NEW"
    age "$(echo loose-old | g hash-object --stdin)" "$OLD"
    # default: unreachable objects go to a cruft pack, the reflogs are expired first
    v gc --prune=$CUT
    cruftshape; packshape; allobjects
    v fsck
    DROP_OUT='^size-pack' v count-objects -v
    # nothing changed: the same cruft pack again
    v gc --prune=$CUT
    cruftshape; allobjects
    # a new loose object joins it; one object of the pack becomes reachable again
    echo loose-newer | g hash-object -w --stdin >/dev/null
    age "$(echo loose-newer | g hash-object --stdin)" "$NEW"
    pick=$(g cat-file --batch-all-objects --batch-check | awk '$2 == "commit" { print $1 }' | while read -r c; do g merge-base --is-ancestor "$c" HEAD 2>/dev/null || echo "$c"; done | head -1)
    g branch resurrected "$pick"
    v gc --prune=$CUT
    cruftshape; packshape; allobjects
    v fsck
    v log --oneline resurrected
    # the other direction: --no-cruft explodes the cruft pack into loose objects again
    g branch -D resurrected -q
    g reflog expire --expire=now --all
    v gc --no-cruft --prune=$CUT
    age_unreachable "$NEW"
    cruftshape; allobjects
    v fsck
    # and back, then everything goes
    v gc --cruft --prune=$CUT
    cruftshape; allobjects
    v gc --cruft --prune=now
    cruftshape; allobjects
    v fsck
    v log --oneline
}

s_gc_cruft_expire() {
    OLD='2026-09-01 00:00:00'; MID='2026-10-10 00:00:00'; NEW='2026-10-20 00:00:00'; CUT=2026-10-05T00:00:00Z
    g config --global gc.reflogExpire never
    g config --global gc.reflogExpireUnreachable never
    v init -q repo; cd repo
    for i in 1 2 3; do echo $i >f$i; v add f$i; tick; v commit -q -m "c$i"; done
    mkt() { rm -f .git/ti; GIT_INDEX_FILE=.git/ti g update-index --add --cacheinfo "100644,$1,$2" >&2; GIT_INDEX_FILE=.git/ti g write-tree; rm -f .git/ti; }
    blob() { local o; o=$(echo "$1" | g hash-object -w --stdin); age "$o" "$2"; echo "$o"; }
    # a chain of old commits, a newer commit on top of the end of it, one with its own old tree
    b1=$(blob b1 "$OLD"); t1=$(mkt "$b1" n1); age "$t1" "$OLD"
    o1=$(g commit-tree "$t1" -m o1); age "$o1" "$OLD"
    b2=$(blob b2 "$OLD"); t2=$(mkt "$b2" n2); age "$t2" "$OLD"
    o2=$(g commit-tree "$t2" -p "$o1" -m o2); age "$o2" "$OLD"
    b3=$(blob b3 "$OLD"); t3=$(mkt "$b3" n3); age "$t3" "$OLD"
    f1=$(g commit-tree "$t3" -p "$o2" -m f1); age "$f1" "$NEW"
    b4=$(blob b4 "$OLD"); t4=$(mkt "$b4" n4); age "$t4" "$OLD"
    f2=$(g commit-tree "$t4" -m f2); age "$f2" "$MID"
    # lone blobs: old, exactly at the cutoff, just after it, and a tag over an old commit
    blob lone-old "$OLD" >/dev/null
    blob lone-at "2026-10-05 00:00:00" >/dev/null
    blob lone-after "2026-10-05 00:00:01" >/dev/null
    seq 1 2000 >big1; (seq 1 2000; echo extra) >big2
    bb=$(g hash-object -w big1); age "$bb" "$NEW"; bb=$(g hash-object -w big2); age "$bb" "$MID"
    rm -f big1 big2
    g tag -a oldtag -m "old tag" "$o1"; g tag -d oldtag >/dev/null
    tg=$(g fsck --unreachable --no-reflogs --no-progress 2>/dev/null | awk '$2 == "tag" { print $3 }'); [ -n "$tg" ] && age "$tg" "$MID"
    v gc --prune=$CUT
    cruftshape; packshape; allobjects
    v fsck
    # a later cutoff expires the middle-aged ones; what the newest still reaches stays
    v gc --prune=2026-10-15T00:00:00Z
    cruftshape; allobjects
    # no expiry at all, and never
    v gc --no-prune
    cruftshape; allobjects
    v gc --prune=never
    cruftshape; allobjects
    # gc.pruneExpire and gc.cruftPacks
    echo cfg-blob | g hash-object -w --stdin >/dev/null; age_unreachable "$NEW"
    g config gc.pruneExpire 2026-10-12T00:00:00Z
    v gc
    cruftshape; allobjects
    g config gc.cruftPacks false
    v gc
    cruftshape; allobjects
    # The gc above loosened the unreachable objects with mtime = wall clock, and git and rivet run
    # seconds apart, so the .mtimes written next would differ by whichever second each one ran in.
    # Pin the loose files' age so the cruft pack records a value both sides can reproduce.
    age_unreachable "$NEW"
    v gc --cruft
    cruftshape; allobjects
    g config gc.cruftPacks true
    v gc --no-cruft
    cruftshape; allobjects
    v fsck
}

s_gc_after_cruft() {
    g init -q repo; cd repo
    echo a >a; g add a; g commit -qm one
    tick; echo b >b; g add b; g commit -qm two
    echo stray | g hash-object -w --stdin >/dev/null
    old=$(echo old-stray | g hash-object -w --stdin)
    touch -d '30 days ago' ".git/objects/${old:0:2}/${old:2}"
    g branch -q doomed; g switch -q doomed; tick; echo c >c; g add c; g commit -qm doomed; g switch -q main; g branch -qD doomed
    g reflog expire --expire=now --all
    g gc -q
    DROP_OUT='^size-pack' v count-objects -v
    objset
    v gc
    # Young unreachable objects stay in a cruft pack, as in git.
    DROP_OUT='^size-pack' v count-objects -v
    objset
    v fsck
    v gc --prune=now
    DROP_OUT='^size-pack' v count-objects -v
    objset
    v fsck
    v log --oneline
}

s_status_nested() {
    v init -q repo; cd repo
    echo k >keep; v add keep; v commit -q -m base
    mkdir nested; git init -q nested; echo n >nested/n; echo m >nested/m
    mkdir -p outer/inner; git init -q outer/inner; echo i >outer/inner/i
    mkdir -p mixed/sub; git init -q mixed/sub; echo s >mixed/sub/s; echo x >mixed/x
    git init -q emptyrepo
    echo u >u
    v status --short
    v status --short --untracked-files=all
    v status --porcelain=v1 -uall
    v status -uall
    v status -unormal
}

s_checkout_unmerged() {
    v init -q repo; cd repo
    echo base >f; echo other >g; v add f g; v commit -q -m base
    v branch side; echo main >f; v commit -q -a -m main
    v switch -q side; echo side >f; v commit -q -a -m side
    v switch -q main
    v merge side
    v checkout -- f
    v checkout -- g
    v checkout -- .
    v checkout HEAD -- f
    v restore f
    v status --short
    cat f
}

s_diff_conflict() {
    v init -q repo; cd repo
    printf 'a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n' >f; echo other >g; printf '1\n2\n3\n' >h
    printf 'x\n' >md; printf 'p\nq\n' >r
    v add f g h md r; v commit -q -m base
    v branch side
    printf 'a\nMAIN\nc\nd\ne\nf\ng\nh\ni\nj\nK1\nl\n' >f; printf '1\n2\nmain3\n' >h; printf 'x\nmain\n' >md; printf 'p\nmain\nq\n' >r
    v commit -q -a -m main
    v switch -q side
    printf 'a\nSIDE\nc\nd\ne\nf\ng\nh\ni\nj\nK2\nl\n' >f; printf '1\n2\nside3\n' >h; git rm -q md; printf 'p\nside\nq\n' >r
    v commit -q -a -m side
    v switch -q main
    v merge side
    v status --short
    v diff
    v diff --name-only
    v diff --name-status
    v diff f
    v diff h md
    v diff -U1 f
    v diff -U0 f
    v diff --exit-code f
    echo $?
    v diff --cached
    # resolve h by taking ours, r by a fresh third version, and leave f/md conflicted
    printf '1\n2\nmain3\n' >h
    printf 'p\nboth\nq\n' >r
    v diff
    v diff --name-only
    echo more >>g
    v diff
    rm md
    v diff -- md
    g add h r f; g rm -q --cached md
    v diff
    v diff --cached
}

s_diff_conflict_shapes() {
    v init -q repo; cd repo
    seq 1 30 >f; seq 1 5 >t
    v add f t; v commit -q -m base
    v branch side
    sed -e '1s/.*/one-main/' -e '15s/.*/fifteen-main/' -e '30s/.*/last-main/' f >f2; mv f2 f
    echo main-add >new; v add new; echo mt >>t
    v commit -q -a -m main
    v switch -q side
    sed -e '1s/.*/one-side/' -e '16s/.*/sixteen-side/' -e '30s/.*/last-side/' f >f2; mv f2 f
    echo side-add >new; v add new; echo st >>t
    v commit -q -a -m side
    v switch -q main
    v merge side
    v diff
    v config merge.conflictStyle diff3
    g checkout -q -m -- f
    v diff f
    sed -i -e '/^<<<<<<<\|^=======\|^>>>>>>>/d' f
    v diff f
    printf 'x\n' >f
    v diff f
    : >f
    v diff f
    printf 'no newline' >f
    v diff f
}

s_stash_untracked() {
    v init -q repo; cd repo
    echo base >a; echo base >b; mkdir d; echo base >d/x; printf '*.o\nbuild/\n' >.gitignore
    v add .; v commit -q -m base
    echo mod >>a; echo u >untr; mkdir -p ud/deep; echo ud >ud/deep/f; echo o >skip.o; mkdir build; echo b >build/out
    v stash push -u
    ls; v status --short --ignored
    v stash list
    v cat-file -p stash
    v cat-file -p 'stash^3'
    v ls-tree -r 'stash^3'
    v stash pop
    v status --short --ignored
    ls
    tick; echo mod2 >>b; v stash push -a -m "with ignored"
    ls; v status --short --ignored
    v ls-tree -r 'stash^3'
    echo existing >untr
    v stash apply
    ls
    rm untr
    v stash apply
    v status --short --ignored
    v stash drop
    v stash -u
    v stash -u
    echo only-untracked >lonely
    v stash push -u -m lonely
    ls
    v stash pop
    ls
}

s_stash_keep_index() {
    v init -q repo; cd repo
    echo base >a; echo base >b; echo base >c; v add .; v commit -q -m base
    echo a2 >>a; v add a; echo a3 >>a; echo b2 >>b; echo new >n; v add n
    v stash push -k
    v status --short
    cat a; cat b
    v stash list
    v cat-file -p 'stash^2'
    v ls-tree -r stash
    v reflog
    v stash drop
    v stash push --keep-index -m kept
    v stash push -u -k
    echo u >untr
    v stash push -u -k
    ls
    v status --short
    v stash list
}

s_stash_pathspec() {
    v init -q repo; cd repo
    echo base >a; echo base >b; echo base >c; mkdir d; echo base >d/x; echo base >d/y; v add .; v commit -q -m base
    echo a2 >>a; v add a; echo a3 >>a; echo b2 >>b; echo c2 >>c; v add c; echo x2 >>d/x; echo y2 >>d/y
    echo u >untr; echo u >d/untr
    v stash push -- a
    v status --short; cat a
    v ls-tree -r 'stash^2'; v ls-tree -r stash
    v cat-file -p stash
    v stash push -m "dir" -- d
    v status --short
    v stash list
    v stash push -- nosuch
    v stash push -m none -- d
    v stash show -p
    v stash show -p 'stash@{1}'
    v stash pop
    v status --short
    v stash push -u -- untr
    ls; v status --short
    v stash push -- b c
    v status --short
    v stash push d/untr
    v stash pop --index
    v status --short
    v reflog
    cd d
    echo y3 >>y
    v stash push -- y
    v status --porcelain
    v stash push -- ../b
}

s_stash_index() {
    v init -q repo; cd repo
    echo base >a; echo base >b; echo base >c; v add .; v commit -q -m base
    echo a2 >>a; v add a; echo a3 >>a; echo b2 >>b; echo new >n; v add n; rm c
    v stash push
    v status --short
    v stash apply
    v status --short
    v reset -q --hard
    v stash apply --index
    v status --short
    cat a; cat n
    v diff --cached
    v reset -q --hard
    echo conflicting >>a; v add a
    vq stash apply --index
    v status --short
    v reset -q --hard
    v stash pop --index
    v status --short
    v stash list
    git checkout -q -- . 
    v stash apply --index
}

s_clean_interactive() {
    v init -q repo; cd repo
    echo k >keep; v add keep; v commit -q -m base
    vin $'q\n' clean -i
    echo 1 >u1; echo 2 >u2; echo 4 >u4.txt; echo 5 >v5; mkdir dd; echo 3 >dd/f
    vin $'q\n' clean -i
    vin $'q\n' clean -i -d
    vin '' clean -i -d
    vin $'h\nq\n' clean -i
    vin $'zz\n\n?\nquit\n' clean -i
    vin $'2\nu*\n\nq\n' clean -i -d
    vin $'3\n1,3\n\nq\n' clean -i -d
    vin $'3\n1-2\n*\n-1\n9\nx\n\n5\n' clean -i -d
    vin $'3\n\n' clean -i -d
    vin $'2\n*\n' clean -i -d
    vin $'4\ny\nn\nYES\n' clean -i
    ls
    vin $'4\ny\n' clean -i -d
    ls
    for i in $(seq 1 23); do echo x >file_$i.txt; done
    vin $'q\n' clean -i
    vin $'3\n1,4,7\n\nq\n' clean -i
    COLUMNS=40 vin $'q\n' clean -i
    vin $'cl\n' clean -n -i
    ls | wc -l
    vin $'c\n' clean -i -q
    ls
    echo 1 >a.log; echo 2 >b.txt; mkdir sub; echo 3 >sub/c.txt
    cd sub
    vin $'c\n' clean -i
    cd ..
    ls
}

s_clean_exclude() {
    v init -q repo; cd repo
    printf '*.o\nbuild/\n' >.gitignore; echo k >keep; v add keep .gitignore; v commit -q -m base
    echo 1 >a.log; echo 2 >b.txt; echo 3 >skip.log; echo 4 >x.o; mkdir build logs keepdir; echo 5 >build/out; echo 6 >logs/l.log; echo 7 >logs/m.txt; echo 8 >keepdir/z.txt
    v clean -n -e '*.log'
    v clean -n -e '*.log' -e 'keepdir/'
    v clean -n -d -e '*.log'
    v clean -n -d -e 'logs'
    v clean -n -x -e 'skip*'
    v clean -n -dx -e 'build/' -e 'x.o'
    v clean -n -X -e 'skip*'
    v clean -n -dX -e 'b*'
    v clean -n -d --exclude='keepdir' --exclude='*.txt'
    v clean -f -e '*.log'
    ls
    v clean -fd -e 'logs/'
    ls
    ls logs
    v clean -fdx -e 'skip.log'
    ls
}

# The delta structure of every pack, as real git reports it, without sizes and offsets that
# depend on the zlib build: "<id> <type> <size> [<depth> <base>]", sorted.
packshape() {
    echo "## packshape" >>"$TRANSCRIPT"
    # Each pack is listed in the order its objects are stored (git's write order); the packs
    # themselves are ordered by their sorted object set, because pack file names differ.
    local idx key blocks; blocks=$(mktemp -d "${TMPDIR:-/tmp}/packshape.XXXXXX")
    for idx in .git/objects/pack/*.idx; do
        [ -e "$idx" ] || continue
        g verify-pack -v "$idx" | awk 'NF == 5 && $1 ~ /^[0-9a-f]+$/ { print $1, $2, $3 } NF == 7 && $1 ~ /^[0-9a-f]+$/ { print $1, $2, $3, $6, $7 }' >"$blocks/cur"
        key=$(sort "$blocks/cur" | sha1sum | cut -c1-40)
        mv "$blocks/cur" "$blocks/$key"
    done
    for key in $(ls "$blocks"); do echo "# pack" >>"$TRANSCRIPT"; cat "$blocks/$key" >>"$TRANSCRIPT"; done
    rm -rf "$blocks"
}

s_gc_delta() {
    g config --global gc.cruftPacks false
    g config --global pack.threads 1
    v init -q repo; cd repo
    seq 1 200 | sed 's/^/line /' >big.txt
    cp big.txt copy.txt
    mkdir src; for i in 1 2 3; do seq 1 80 | sed "s/^/mod$i /" >src/m$i.c; done
    v add .; v commit -q -m one
    for n in 2 3 4 5 6 7 8 9 10 11 12; do
        tick; sed -i "$((n * 10))s/.*/changed at rev $n/" big.txt; echo "extra $n" >>copy.txt
        sed -i "5s/.*/m1 rev $n/" src/m1.c; v commit -q -a -m "rev $n"
    done
    v tag -a v1 -m annotated
    v gc
    packshape
    tick; echo more >>big.txt; v commit -q -a -m again
    v gc
    packshape
    v fsck
    DROP_OUT='^size-pack' v count-objects -v
    objset >>"$TRANSCRIPT"
}

s_gc_delta_shapes() {
    g config --global gc.cruftPacks false
    g config --global pack.threads 1
    v init -q repo; cd repo
    mkdir -p lib/a lib/b docs
    for i in $(seq 1 40); do seq 1 $((20 + i)) | sed "s/^/file $i row /" >lib/a/f$i.txt; done
    for i in $(seq 1 15); do seq 1 60 | sed "s/^/b$i /" >lib/b/g$i.c; done
    awk 'BEGIN { srand(7); for (i = 0; i < 3000; i++) printf "%d %d lorem ipsum %d\n", i, int(rand() * 100000), int(rand() * 7) }' >docs/big.txt
    awk 'BEGIN { for (i = 0; i < 4000; i++) print "same same same same" }' >docs/rep.txt
    head -c 30000 /dev/zero | tr '\0' 'A' >docs/blob.bin
    v add .; v commit -q -m base
    for n in $(seq 2 64); do
        tick
        sed -i "$((n * 3 % 3000 + 1))s/.*/edited at $n/" docs/big.txt
        echo "n $n" >>lib/a/f$((n % 40 + 1)).txt
        if [ $((n % 5)) = 0 ]; then echo "row $n" >>docs/rep.txt; fi
        if [ $((n % 7)) = 0 ]; then printf 'B%s' "$n" >>docs/blob.bin; fi
        if [ $((n % 9)) = 0 ]; then git rm -q -f lib/b/g$((n / 9)).c; fi
        v commit -q -a -m "rev $n"
    done
    v branch topic HEAD~20
    v tag light HEAD~5
    v tag -a v1 -m annotated HEAD~30
    v gc
    packshape
    tick; echo tail >>docs/big.txt; v commit -q -a -m tail
    v switch -q topic; tick; echo topic >>lib/a/f1.txt; v commit -q -a -m topic; v switch -q main
    v gc
    packshape
    v fsck
    v log --oneline --all | head -3
}

# A history with branches, merges, tags (annotated, lightweight, nested) and several authors.
query_repo() {
    v init -q repo; cd repo
    echo 1 >a; mkdir d; echo 1 >d/b; v add .; v commit -q -m "one"
    tick; echo 2 >>a; v commit -q -a -m "two"; v tag light1
    tick; echo 3 >>a; v commit -q -a -m "three"; v tag -a -m "ann one" v1.0
    v branch side
    tick; echo 4 >>a; GIT_AUTHOR_NAME="Zed Z" GIT_AUTHOR_EMAIL=z@x v commit -q -a -m "four"
    v switch -q side
    tick; echo s1 >s; v add s; GIT_AUTHOR_NAME="Amy A" GIT_AUTHOR_EMAIL=amy@x v commit -q -m "side one"
    tick; echo s2 >>s; v commit -q -a -m "side two"; v tag -a -m "side tag" side-ann
    v switch -q main
    tick; v merge -q --no-ff side -m "merge side" >/dev/null
    tick; echo 5 >>a; GIT_AUTHOR_NAME="Zed Z" GIT_AUTHOR_EMAIL=z@x v commit -q -a -m "five"
    v tag light-latest
    tick; echo 6 >>d/b; v commit -q -a -m "six"
}

s_rev_list() {
    query_repo
    v rev-list HEAD
    v rev-list --count HEAD
    v rev-list --max-count=3 HEAD
    v rev-list -n 2 --reverse HEAD
    v rev-list --reverse HEAD
    v rev-list main..side
    v rev-list side..main
    v rev-list v1.0..HEAD
    v rev-list HEAD ^side
    v rev-list --count main...side
    v rev-list main...side
    v rev-list --all
    v rev-list --all --count
    v rev-list --first-parent HEAD
    v rev-list --objects HEAD
    v rev-list --objects v1.0..HEAD
    v rev-list --objects --all
    v rev-list --objects --count HEAD
    v rev-list --objects -n 2 HEAD
    v rev-list --objects --reverse -n 3 HEAD
    v rev-list nosuch
    v rev-list light1 v1.0
    v rev-list --count side-ann
}

s_show_ref() {
    query_repo
    v branch other side
    v show-ref
    v show-ref --heads
    v show-ref --tags
    v show-ref -d
    v show-ref -d --tags
    v show-ref --head
    v show-ref main
    v show-ref heads/main
    v show-ref refs/heads/main
    v show-ref v1.0 light1
    v show-ref nosuch
    v show-ref -s main
    v show-ref -d -s --tags
    v show-ref --verify refs/heads/main
    v show-ref --verify refs/heads/main refs/tags/v1.0
    v show-ref --verify -d refs/tags/v1.0
    v show-ref --verify main
    v show-ref --verify refs/heads/nosuch
    v show-ref --verify
    v show-ref --verify HEAD
    v show-ref --quiet --verify refs/heads/nosuch
    v show-ref --quiet --verify refs/heads/main
    v show-ref --quiet main
    g pack-refs --all
    v show-ref -d
}

s_for_each_ref() {
    query_repo
    v branch other side; v tag same-name; v branch same-name
    v for-each-ref
    v for-each-ref refs/heads
    v for-each-ref refs/tags/
    v for-each-ref 'refs/heads/s*'
    v for-each-ref --format='%(refname)'
    v for-each-ref --format='%(refname:short)'
    v for-each-ref --format='%(objectname) %(objectname:short) %(objectname:short=10) %(objecttype) %(objectsize)'
    v for-each-ref --format='%(refname:short) -> %(*objectname) %(*objecttype)' refs/tags
    v for-each-ref --format='%(refname:lstrip=2)|%(refname:strip=1)|%(refname:rstrip=1)'
    v for-each-ref --format='%(HEAD) %(refname:short) 100%%'
    v for-each-ref --count=2
    v for-each-ref --sort=-refname --format='%(refname)'
    v for-each-ref --sort=objectname --format='%(objectname) %(refname)'
    v for-each-ref --format='%(bogus)'
    v for-each-ref --sort=bogus
    git symbolic-ref refs/remotes/origin/HEAD refs/heads/main 2>/dev/null
    git update-ref refs/remotes/origin/main HEAD
    git symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main
    v for-each-ref --format='%(refname) %(objectname:short) [%(symref)]' refs/remotes
    v for-each-ref --format='%(refname:short)' refs/remotes
    g pack-refs --all
    v for-each-ref --format='%(refname:short) %(objecttype)'
}

s_merge_base() {
    query_repo
    v merge-base main side
    v merge-base side main
    v merge-base --all main side
    v merge-base HEAD v1.0
    v merge-base --is-ancestor v1.0 HEAD
    echo "rc=$?"
    v merge-base --is-ancestor HEAD v1.0
    echo "rc=$?"
    v merge-base --is-ancestor HEAD HEAD
    v merge-base HEAD~1 HEAD
    v merge-base nosuch HEAD
    vq merge-base HEAD
    # unrelated histories and criss-cross merges
    git checkout -q --orphan lone; git rm -rfq . ; echo x >x; git add x; tick; git commit -qm lone
    v merge-base main lone
    v merge-base --is-ancestor main lone
    git checkout -q main
    git checkout -q -b x1 v1.0; echo x1 >x1; git add x1; tick; git commit -qm x1
    git checkout -q -b y1 v1.0; echo y1 >y1; git add y1; tick; git commit -qm y1
    git checkout -q x1; tick; git merge -q --no-edit y1 -m mx >/dev/null
    git checkout -q y1; tick; git merge -q --no-edit x1~0 -m my >/dev/null 2>&1
    git checkout -q -b x2 x1; echo x2 >x2; git add x2; tick; git commit -qm x2
    git checkout -q -b y2 y1; echo y2 >y2; git add y2; tick; git commit -qm y2
    git checkout -q x2; git merge -q --no-edit y1 >/dev/null 2>&1
    git checkout -q y2; git merge -q --no-edit x1 >/dev/null 2>&1
    v merge-base x2 y2
    v merge-base --all x2 y2
}

s_describe() {
    query_repo
    v describe
    v describe --tags
    v describe --long
    v describe --tags --long
    v describe --always
    v describe v1.0
    v describe --long v1.0
    v describe v1.0~1
    v describe --tags v1.0~1
    v describe --tags v1.0~2
    v describe --always v1.0~2
    v describe v1.0~2
    v describe --exact-match v1.0
    v describe --exact-match HEAD
    v describe --tags --exact-match light1
    v describe --abbrev=10 --tags
    v describe --abbrev=4
    v describe side
    v describe --tags side
    v describe side~1
    v describe light-latest
    v describe --tags light-latest
    v describe nosuch
    # several tags on one commit: annotated beats lightweight, newer annotated beats older
    v tag -a -m "x" ann-a "v1.0^{commit}"
    tick; v tag -a -m "y" ann-b "v1.0^{commit}"
    v tag zlight v1.0
    v describe v1.0
    v describe --tags v1.0
    v describe --tags v1.0~0
    v describe main~3
    v describe --tags main~3
    # a repository with no tags
    cd ..; v init -q plain; cd plain; echo x >x; v add x; v commit -q -m x
    v describe
    v describe --tags
    v describe --always
    v tag lw
    v describe
    v describe --tags
}

s_shortlog() {
    query_repo
    v shortlog HEAD
    v shortlog -s HEAD
    v shortlog -n HEAD
    v shortlog -sn HEAD
    v shortlog -sne HEAD
    v shortlog -e HEAD
    v shortlog -s -n -e HEAD
    v shortlog -sc HEAD
    v shortlog -s side
    v shortlog -s main~2
    v shortlog -s nosuch
    GIT_AUTHOR_NAME="Amy A" GIT_AUTHOR_EMAIL=other@x tick
    echo 7 >>a; GIT_AUTHOR_NAME="Amy A" GIT_AUTHOR_EMAIL=other@x v commit -q -a -m "seven"
    v shortlog -se HEAD
    v shortlog -s HEAD
    v shortlog -sn HEAD
    GIT_COMMITTER_NAME="Other C" GIT_COMMITTER_EMAIL=oc@x v commit -q --allow-empty -m "empty by other"
    v shortlog -sc HEAD
    v shortlog -sce HEAD
}

rebase_repo() {
    v init -q repo; cd repo
    printf 'a1\na2\na3\n' >a; echo f >f; v add .; v commit -q -m A
    v branch topic
    tick; echo b >>a; v commit -q -a -m B
    tick; echo c >>a; v commit -q -a -m C
    v switch -q topic
    tick; echo d >d; v add d; v commit -q -m D
    tick; echo "f topic" >>f; v commit -q -a -m E
    tick; echo g >g; v add g; v commit -q -m G
    v switch -q main
}

s_rebase_basic() {
    rebase_repo
    v rebase main topic
    v log --oneline --all
    v reflog
    v reflog topic
    cat .git/ORIG_HEAD
    ls .git/rebase-merge
    v rebase main
    v rebase main topic
    v rebase main
    v switch -q topic
    v rebase main
    v rebase topic
    v switch -q main
    v rebase topic
    v log --oneline
    v reflog
    v rebase nosuch
    v rebase
    v rebase --continue
    v rebase --abort
    v rebase --skip
}

s_rebase_onto() {
    rebase_repo
    v switch -q topic
    v rebase --onto main HEAD~2
    v log --oneline
    v reflog
    v switch -q -c t2 "$(g rev-parse topic@{3})"
    v log --oneline
    v rebase --onto main HEAD~1
    v log --oneline
    v reflog
    v switch -q main
    v branch t3 topic
    v rebase --onto main~1 main t3
    v log --oneline t3
    git checkout -q --detach t3
    v rebase --onto main t3~1
    v log --oneline
    v status --short
    v reflog
}

s_rebase_cherry_and_merges() {
    v init -q repo; cd repo
    echo 0 >base; v add .; v commit -q -m base
    v branch topic
    tick; echo 1 >x; v add x; v commit -q -m "add x"
    tick; echo main >m; v add m; v commit -q -m "main only"
    v switch -q topic
    tick; echo 1 >x; v add x; v commit -q -m "add x (same patch)"
    tick; echo t >t; v add t; v commit -q -m "topic t"
    v branch side
    v switch -q side
    tick; echo s >s; v add s; v commit -q -m "side s"
    v switch -q topic
    tick; echo u >u; v add u; v commit -q -m "topic u"
    tick; v merge -q --no-ff side -m "merge side" >/dev/null
    tick; echo v >v; v add v; v commit -q -m "topic v"
    v log --oneline
    v rebase main
    v log --oneline
    v reflog
    v ls-files
}

s_rebase_quiet_skipped() {
    v init -q repo; cd repo
    echo 0 >base; v add .; v commit -q -m base
    v branch topic
    tick; echo m >m; v add m; v commit -q -m "main first"
    tick; echo 1 >x; v add x; v commit -q -m "add x"
    v switch -q topic
    tick; echo 1 >x; v add x; v commit -q -m "add x (same patch)"
    tick; echo t >t; v add t; v commit -q -m "topic t"
    v rebase -q main
    v log --oneline
    v reflog
}

s_rebase_dirty() {
    rebase_repo
    v switch -q topic
    echo x >>d
    v rebase main
    v rebase main topic
    git add d
    v rebase main
    echo y >>d
    v rebase main
    git checkout -q -- d; git reset -q --hard
    echo untracked >g.new
    v rebase main
    v log --oneline
    v switch -q main
    echo z >>a
    v rebase topic
    v rebase main
}

s_rebase_conflict() {
    rebase_repo
    tick; echo "main f" >>f; v commit -q -a -m "main f"
    v switch -q topic
    tick; echo "more" >>d; v commit -q -a -m "D2"
    v rebase main
    echo "rc=$?"
    v status
    v status --short
    v diff
    v rebase --continue
    v rebase main
    cat f
    printf 'f\nresolved\n' >f
    v status
    v rebase --continue
    v log --oneline
    v reflog
    v status
    cat .git/ORIG_HEAD
    ls .git/rebase-merge
}

s_rebase_skip_abort() {
    rebase_repo
    tick; echo "main f" >>f; v commit -q -a -m "main f"
    v switch -q topic
    tick; echo "more" >>f; v commit -q -a -m "F2"
    v rebase main
    v rebase --skip
    v log --oneline
    v reflog
    v status
    cat f
    v reset -q --hard "$(g rev-parse topic@{1})"
    v rebase main
    v rebase --abort
    v log --oneline
    v status
    v reflog
    cat .git/ORIG_HEAD
    git checkout -q --detach topic
    v rebase main
    v rebase --abort
    v status --short
    v rebase --abort
    printf 'f\nresolved\n' >f
    v rebase main
    printf 'f\nresolved\n' >f
    git add f
    v rebase --continue
    v log --oneline
}

s_rebase_multi_conflict() {
    v init -q repo; cd repo
    printf '1\n2\n3\n' >f; printf 'x\ny\n' >g; echo k >keep; v add .; v commit -q -m base
    v branch topic
    tick; printf '1\nmain2\n3\n' >f; printf 'x\nmainy\n' >g; v commit -q -a -m "main edits"
    v switch -q topic
    tick; printf '1\ntopic2\n3\n' >f; v commit -q -a -m "topic f"
    tick; echo extra >extra; v add extra; v commit -q -m "topic extra"
    tick; printf 'x\ntopicy\n' >g; v commit -q -a -m "topic g"
    tick; git rm -q keep; v commit -q -m "topic removes keep"
    v rebase main
    v status --short
    cat f
    git checkout -q --ours f 2>/dev/null; git add f
    v rebase --continue
    v status
    v diff
    git checkout -q --theirs g 2>/dev/null; git add g
    v status --short
    v rebase --continue
    v log --oneline
    v reflog
    v status
    ls
    cat f g
    # a conflict resolved to the base version leaves nothing to commit
    v reset -q --hard "$(g rev-parse topic@{5})"
    v rebase main
    git checkout -q --ours f; git add f
    v rebase --continue
    v rebase --skip
    v status --short
    v rebase --skip
    v log --oneline
}

s_grep() {
    v init -q repo; cd repo
    mkdir -p src docs
    printf 'alpha beta\nfoo bar baz\nFOO upper\nfoobar\nthe end\n' >a.txt
    printf 'int main() {\n  return foo(1);\n}\nstatic int foo(int x) { return x*2; }\n' >src/main.c
    printf 'one\ntwo foo\nthree\nfour\nfive foo\nsix\nseven\neight\nnine foo\n' >docs/notes.md
    printf 'bin\0foo\n' >blob.bin
    printf 'no newline foo' >nonl
    echo untracked foo >untracked
    v add a.txt src docs blob.bin nonl; v commit -q -m one
    v grep foo
    v grep -n foo
    v grep -in foo
    v grep -c foo
    v grep -l foo
    v grep -L foo
    v grep -w foo
    v grep -v -n foo a.txt
    v grep -F 'x*2'
    v grep 'x*2'
    v grep -E 'foo|FOO' a.txt
    v grep 'foo\|FOO' a.txt
    v grep -E '^(foo|the)' a.txt
    v grep '^foo' a.txt
    v grep -E 'o{2}' a.txt
    v grep -e alpha -e end a.txt
    v grep -n -C1 foo docs/notes.md
    v grep -n -A1 foo docs/notes.md
    v grep -n -B2 five docs/notes.md
    v grep -n -e two -e nine -C1 docs/notes.md
    v grep -h foo a.txt
    v grep -a foo blob.bin
    v grep -I foo
    v grep -q foo
    v grep -q nonexistent
    v grep nonexistent
    v grep 'fo(' a.txt
    v grep -E 'fo(' a.txt
    v grep '[[:upper:]]\{3\}' a.txt
    v grep -- foo src
    v grep foo -- docs
    v grep foo HEAD
    v grep -n foo HEAD -- src
    echo "foo modified" >>a.txt
    v grep -n foo a.txt
    v grep -n --cached foo a.txt
    rm docs/notes.md
    v grep -c foo
    v grep -c --cached foo
    cd src
    v grep foo
    v grep -n foo ../a.txt
    v grep -n foo -- ../docs
    cd ..
    v grep -E
    v grep
}

s_blame() {
    v init -q repo; cd repo
    printf 'one\ntwo\nthree\nfour\nfive\n' >f.txt
    v add f.txt; v commit -q -m "root"
    tick; printf 'one\ntwo\nTHREE\nfour\nfive\nsix\n' >f.txt
    GIT_AUTHOR_NAME="Bob Longname" GIT_AUTHOR_EMAIL=bob@x v commit -q -a -m "bob edits"
    v branch side
    tick; printf 'zero\none\ntwo\nTHREE\nfour\nfive\nsix\n' >f.txt
    GIT_AUTHOR_NAME="Al" GIT_AUTHOR_EMAIL=al@x v commit -q -a -m "al prepends"
    v switch -q side
    tick; printf 'one\ntwo\nTHREE\nfour\nFIVE\nsix\nseven' >f.txt
    GIT_AUTHOR_NAME="Cy" GIT_AUTHOR_EMAIL=cy@x v commit -q -a -m "cy on side"
    v switch -q main
    tick; g merge -q side -m "merge side" >/dev/null 2>&1 || { printf 'zero\none\ntwo\nTHREE\nfour\nFIVE\nsix\nseven' >f.txt; git add f.txt; git commit -qm "merge side"; }
    v blame f.txt
    v blame -s f.txt
    v blame -e f.txt
    v blame -l f.txt
    v blame -L 2,4 f.txt
    v blame -L 3 f.txt
    v blame -L ,2 f.txt
    v blame -L 2,+2 f.txt
    v blame -L 4,-2 f.txt
    v blame -L 1,2 -L 6,7 f.txt
    v blame -L 20,30 f.txt
    v blame HEAD~1 f.txt
    v blame HEAD~2 -- f.txt
    v blame side f.txt
    v blame nosuch.txt
    v blame HEAD nosuch.txt
    v blame nosuchrev f.txt
    # a rename with edits
    tick; git mv f.txt g.txt; v commit -q -m "rename"
    tick; sed -i 's/^two$/TWO/' g.txt; GIT_AUTHOR_NAME="Dee" GIT_AUTHOR_EMAIL=dee@x v commit -q -a -m "dee"
    v blame g.txt
    v blame -f g.txt
    mkdir sub; cp g.txt sub/h.txt; v add sub; tick; v commit -q -m "copy to sub"
    cd sub
    v blame h.txt
    v blame ../g.txt
}

s_blame_edits() {
    v init -q repo; cd repo
    awk 'BEGIN { for (i = 1; i <= 40; i++) print "line " (i % 7) " body " int(i / 5) }' >t.txt
    v add t.txt; v commit -q -m base
    for n in $(seq 1 14); do
        tick
        awk -v n=$n 'BEGIN { srand(n * 31) } { r = int(rand() * 10);
            if (r == 0) next;
            if (r == 1) { print $0; print "dup " n " " NR; next }
            if (r == 2) { print "chg " n " " NR; next }
            print $0 }' t.txt >t.new && mv t.new t.txt
        GIT_AUTHOR_NAME="Author $((n % 4))" GIT_AUTHOR_EMAIL="a$((n % 4))@x" v commit -q -a -m "edit $n"
        if [ $((n % 5)) = 0 ]; then v branch -f keep$n; fi
    done
    v blame t.txt
    v blame keep10 t.txt
    v blame -L 5,15 keep5 t.txt
    v blame -s -L 30,+5 t.txt
}

s_apply() {
    v init -q repo; cd repo
    seq 1 30 | sed 's/^/line /' >a.txt
    printf 'x\ny\nz\n' >b.txt
    printf 'no newline' >nonl.txt
    mkdir d; echo d1 >d/f; echo old >old.txt; printf '#!/bin/sh\n' >run.sh
    v add .; v commit -q -m base
    # modify, add, delete, rename, mode change, no-newline changes
    sed -i -e '5s/.*/FIVE/' -e '20s/.*/TWENTY/' a.txt
    echo "more" >>b.txt
    printf 'no newline but longer' >nonl.txt
    echo new >added.txt; mkdir -p d/e; echo deep >d/e/g
    git rm -q -f old.txt
    git mv b.txt c.txt; echo "renamed edit" >>c.txt
    chmod +x run.sh
    git add -A
    g diff --cached -M >../full.patch
    g diff --cached -M --no-prefix >../noprefix.patch
    g reset -q --hard
    v apply --check ../full.patch
    v status --short
    v apply ../full.patch
    v status --short
    cat a.txt | sed -n '5p;20p'; cat c.txt; cat nonl.txt; echo; ls; ls -l run.sh | cut -c1-10
    v apply ../full.patch
    v apply -R --check ../full.patch
    v apply -R ../full.patch
    v status --short
    ls
    v apply -p0 ../noprefix.patch
    v status --short
    g reset -q --hard; g clean -fdq
    v apply --index ../full.patch
    v status --short
    v diff --cached --stat
    g reset -q --hard; g clean -fdq
    v apply --cached ../full.patch
    v status --short
    v diff --stat
    g reset -q --hard; g clean -fdq
    # offsets: lines inserted above the hunks
    sed -i '1i top1\ntop2\ntop3' a.txt
    v apply ../full.patch
    v status --short
    sed -n '1,3p;8p;23p' a.txt
    g reset -q --hard; g clean -fdq
    # context mismatch
    sed -i '5s/.*/changed/' a.txt
    v apply ../full.patch
    v apply --check ../full.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # missing file, existing target
    git rm -q -f --cached d/f; rm d/f; v apply ../full.patch
    g reset -q --hard
    echo present >added.txt
    v apply ../full.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # stdin and plain diff -u
    cp a.txt ../a.orig; sed -i '10s/.*/TEN/' a.txt; (cd ..; diff -u a.orig repo/a.txt | sed -e '1s#a.orig#a.txt#' -e '2s#repo/a.txt#a.txt#' >plain.patch); g checkout -q -- a.txt
    vin "$(cat ../full.patch)"$'\n' apply
    g reset -q --hard; g clean -fdq
    v apply -p0 ../plain.patch
    v status --short
    g checkout -q -- a.txt
    v apply nosuch.patch
    vin '' apply
    vin $'garbage\n' apply
}

s_apply_stat() {
    v init -q repo; cd repo
    seq 1 30 | sed 's/^/line /' >a.txt
    printf 'x\ny\n' >b.txt; echo old >old.txt; printf '#!/bin/sh\n' >run.sh
    D=some/very/long/directory/name/for/testing; mkdir -p $D
    seq 1 100 >$D/longfile_name_here.txt
    seq 1 6000 >big.txt; printf 'k\n' >mixed.txt
    v add .; v commit -q -m base
    sed -i '5s/.*/FIVE/' a.txt; echo more >>b.txt; git rm -q -f old.txt; chmod +x run.sh
    seq 1 100 | sed 's/^/Z/' >$D/longfile_name_here.txt
    seq 1 6000 | sed 's/^/X/' >big.txt
    echo new >added.txt; printf 'caf\303\251\n' >"caf\303\251.txt"; : >empty.txt
    git add -A
    g diff --cached >../all.patch
    g diff --cached -R >../rev.patch
    g diff --cached --no-prefix -- a.txt b.txt added.txt >../few.patch
    g reset -q --hard; g clean -fdq
    v apply --stat ../all.patch
    v apply --numstat ../all.patch
    v apply --summary ../all.patch
    v apply --stat --numstat --summary ../all.patch
    v apply --summary --numstat --stat ../all.patch
    v apply -z --numstat ../all.patch
    v apply -z --summary ../all.patch
    v apply -R --stat --summary ../rev.patch
    v apply -R --numstat ../all.patch
    v apply --stat -p0 ../few.patch
    v status --short
    # inspecting does not apply; --apply does, --check does not
    v apply --stat --check ../all.patch
    v status --short
    v apply --stat --apply ../all.patch
    v status --short
    cat a.txt | sed -n '5p'; ls
    g reset -q --hard; g clean -fdq
    v apply --numstat --apply --index ../all.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # a patch that does not fit prints no statistics when it is checked
    sed -i '5s/.*/changed/' a.txt
    v apply --stat ../all.patch
    v apply --stat --check ../all.patch
    v apply --stat --apply ../all.patch
    v status --short
    # an empty or unreadable patch
    vin '' apply --stat
    v apply --stat nosuch.patch
}

s_apply_stat_renames() {
    v init -q repo; cd repo
    mkdir -p d/e d/x
    seq 1 20 >d/e/f.txt; seq 1 20 >d/x/g.txt; seq 1 20 >h.txt; seq 1 20 >cp.txt
    seq 1 20 >m.sh; seq 1 20 >n.sh; printf '\000\001bin' >bin.dat
    v add .; v commit -q -m base
    git mv d/e/f.txt d/x/f.txt; git mv d/x/g.txt d/e/gg.txt; git mv h.txt "t ab.txt"
    sed -i '3s/.*/three/' d/x/f.txt
    cp cp.txt cp2.txt; sed -i 1d cp2.txt
    git mv m.sh m2.sh; chmod +x m2.sh
    chmod +x n.sh
    printf '\000\002bin!' >bin.dat
    git add -A
    g diff --cached -M -C --find-copies-harder HEAD >../ren.patch
    g reset -q --hard
    v apply --stat --numstat --summary ../ren.patch
    v apply -z --numstat ../ren.patch
    v apply -R --stat --summary ../ren.patch
    v apply --stat --summary --check ../ren.patch
    # binary files are listed as Bin / -
    v apply --numstat ../ren.patch
}

s_apply_reject() {
    v init -q repo; cd repo
    seq 1 60 >a.txt; seq 1 10 >b.txt; echo q >c.txt; printf 'tail one\ntail two' >nonl.txt
    mkdir d; seq 1 30 >d/e.txt; seq 1 30 >mv.txt
    v add .; v commit -q -m base
    sed -i -e '5s/.*/FIVE/' -e '50s/.*/FIFTY/' a.txt; sed -i '3s/.*/three/' b.txt; echo qq >>c.txt
    printf 'tail one\ntail 2' >nonl.txt; sed -i '10s/.*/TEN/;25s/.*/TWENTY-FIVE/' d/e.txt
    git mv mv.txt moved.txt; sed -i '28s/.*/edited/' moved.txt
    git add -A; g diff --cached -M >../rj.patch; g reset -q --hard
    cp -r ../repo ../clean.copy 2>/dev/null
    # all hunks fit
    v apply --reject ../rj.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # the second hunk of a.txt does not fit; the other files still apply
    sed -i '50s/.*/other/' a.txt
    v apply --reject ../rj.patch
    v status --short
    cat a.txt.rej
    g reset -q --hard; g clean -fdq
    # nothing fits
    echo zzz >a.txt; echo yyy >b.txt; echo xxx >c.txt; printf 'no\nnl' >nonl.txt; echo w >d/e.txt; echo v >mv.txt
    v apply --reject ../rj.patch
    v status --short
    cat a.txt.rej b.txt.rej nonl.txt.rej d/e.txt.rej
    g reset -q --hard; g clean -fdq
    # an existing .rej is overwritten
    sed -i '50s/.*/other/' a.txt; echo PRE >a.txt.rej
    v apply --reject ../rj.patch
    cat a.txt.rej
    g reset -q --hard; g clean -fdq
    # --check with --reject reports and changes nothing
    sed -i '50s/.*/other/' a.txt
    v apply --reject --check ../rj.patch
    v status --short
    # a missing file is an error, the others still go in
    rm b.txt
    v apply --reject ../rj.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # --index: the work tree file differs from the index
    sed -i '50s/.*/other/' a.txt
    v apply --reject --index ../rj.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # reverse
    v apply --reject -R ../rj.patch
    v status --short
    cat a.txt.rej
    g reset -q --hard; g clean -fdq
    # without --reject nothing is written, and -R names the line of the reversed hunk
    sed -i '50s/.*/other/' a.txt
    v apply ../rj.patch
    v apply -R ../rj.patch
    v status --short
    # the same file twice in one patch: the second reject file replaces the first
    g reset -q --hard
    sed -i '50s/.*/other/' a.txt
    cat ../rj.patch >../dup.patch
    awk '/^diff --git a\/b.txt/ { exit } { print }' ../rj.patch | sed 's/FIVE/FIVE2/;s/FIFTY/FIFTY2/' >>../dup.patch
    v apply --reject ../dup.patch
    cat a.txt.rej
    v status --short
}

s_apply_binary() {
    v init -q repo; cd repo
    printf '\000\001abc' >bin.dat; seq 1 5 >t.txt; printf '\000gone' >gone.dat
    v add .; v commit -q -m base
    printf '\000\002xyz!' >bin.dat; sed -i 3d t.txt; printf '\000new' >newbin.dat; git rm -q -f gone.dat
    git add -A
    g diff --cached >../plain.patch
    g diff --cached --full-index >../full.patch
    g diff --cached --binary >../gitbin.patch
    # a post-image blob this repository has never seen
    g init -q ../other; cp bin.dat ../bin.keep; cp t.txt ../t.keep
    (cd ../other && cp ../repo/.git/config .git/config 2>/dev/null; printf '\000\001abc' >bin.dat; seq 1 5 >t.txt; printf '\000gone' >gone.dat
     git add . && git commit -q -m base && printf '\000other post image' >bin.dat && git add -A && git diff --cached --full-index >../foreign.patch)
    g reset -q --hard; g clean -fdq
    # no usable index line: refused, nothing silently skipped
    v apply ../plain.patch
    v apply --check ../plain.patch
    v apply --index ../plain.patch
    v apply --cached ../plain.patch
    v apply -R ../plain.patch
    v apply --reject ../plain.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # inspecting binary patches works
    v apply --stat --numstat --summary ../plain.patch
    v apply --stat --numstat --summary ../full.patch
    # full index ids: the post-image comes from the object database
    v apply --check ../full.patch
    v apply ../full.patch
    v status --short
    cat bin.dat | od -An -c
    g reset -q --hard; g clean -fdq
    v apply --index ../full.patch
    v status --short
    g reset -q --hard; g clean -fdq
    v apply --cached ../full.patch
    v status --short
    g reset -q --hard; g clean -fdq
    v apply -R ../full.patch
    # the pre-image is not what the patch was made against
    printf '\000zzz' >bin.dat
    v apply ../full.patch
    v status --short
    g checkout -q -- bin.dat
    # the post-image blob is not in this repository
    v apply ../foreign.patch
    v apply --reject ../foreign.patch
    v status --short
}

s_apply_three_way() {
    v init -q repo; cd repo
    seq 1 20 >a.txt; seq 1 10 >b.txt; seq 1 12 >c.txt; seq 1 8 >d.txt; seq 1 9 >gone.txt; seq 1 15 >mv.txt
    v add .; v commit -q -m base
    sed -i '5s/.*/FIVE/' a.txt; sed -i '3s/.*/three/' b.txt; sed -i '6s/.*/six/' c.txt; sed -i '2s/.*/two/' d.txt
    echo brand >new.txt; git rm -q gone.txt; git mv mv.txt moved.txt; sed -i '10s/.*/ten/' moved.txt
    git add -A
    g diff --cached -M >../tw.patch
    sed '/^index /d' ../tw.patch >../noidx.patch
    sed 's/^index [0-9a-f]*\.\./index 1234567../' ../tw.patch >../badblob.patch
    g reset -q --hard; g clean -fdq
    # untouched tree: git merges instead of applying directly, and says so
    v apply --3way ../tw.patch
    v status --short
    v ls-files -s
    g reset -q --hard; g clean -fdq
    # local commit that conflicts in a.txt, merges in b.txt and moved.txt, leaves the rest alone
    tick; sed -i '1s/.*/ONE/;5s/.*/five-local/' a.txt; sed -i '9s/.*/nine/' b.txt; sed -i '1s/.*/uno/' mv.txt
    v commit -q -a -m local
    v apply --3way ../tw.patch
    v status --short
    v ls-files -s
    cat a.txt | sed -n 1,9p; sed -n 1,4p b.txt; sed -n 1,3p moved.txt; sed -n 10p moved.txt; ls
    g reset -q --hard; g clean -fdq
    v apply --3way --check ../tw.patch
    v status --short
    v apply --3way --cached ../tw.patch
    v status --short
    v ls-files -s
    g reset -q --hard; g clean -fdq
    v apply --3way --stat ../tw.patch
    v apply -R --3way ../tw.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # without index lines or blobs there is nothing to merge: fall back to direct application
    v apply --3way ../noidx.patch
    v status --short
    g reset -q --hard; g clean -fdq
    v apply --3way ../badblob.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # the work tree does not match the index
    echo dirty >>b.txt
    v apply --3way ../tw.patch
    v status --short
    g reset -q --hard; g clean -fdq
    # a patch that applies directly at the preimage, as a plain diff (no index line) still works
    g checkout -q HEAD~1
    v apply --3way ../noidx.patch
    v status --short
    g reset -q --hard; g clean -fdq
    v apply --3way --reject ../tw.patch
    # the index line names a blob the hunks were not made against
    oth=$(g rev-parse --short=7 :b.txt); old=$(g rev-parse --short=7 :a.txt)
    sed "s/^index $old/index $oth/" ../tw.patch >../wrongblob.patch
    v apply --3way ../wrongblob.patch
    v status --short
}

s_format_patch() {
    v init -q repo; cd repo
    seq 1 20 >a; printf '#!/bin/sh\n' >run.sh
    v add .; v commit -q -m "First: add [a] file

Body paragraph one.

Second paragraph"
    tick; echo 21 >>a; echo b >b; v add b; v commit -q -a -m "Second commit with a rather long subject line that goes past the sixty-four character mark!"
    tick; git rm -q b; v commit -q -m "Remove b..."
    tick; git mv a renamed; echo more >>renamed; chmod +x run.sh; v commit -q -a -m "Rename and chmod

with a body"
    tick; printf 'no newline' >nonl; git add nonl; v commit -q -m "no newline"
    tick; printf 'no newline changed' >nonl; v commit -q -a -m "multi
line
subject"
    v branch side HEAD~2
    v switch -q side
    tick; echo side >s; v add s; v commit -q -m "side"
    v switch -q main
    tick; g merge -q --no-ff side -m "merge side" >/dev/null
    tick; echo tail >>renamed; v commit -q -a -m "after merge"
    v format-patch --stdout --no-signature -1
    v format-patch --stdout --no-signature -2
    v format-patch --stdout --no-signature --root -n -1 HEAD~4
    v format-patch --stdout --no-signature HEAD~3
    v format-patch --stdout --no-signature main~4..main
    v format-patch --stdout --no-signature HEAD~4..HEAD~2
    v format-patch --stdout --no-signature -3 HEAD~1
    v format-patch --stdout --no-signature --start-number 7 -2
    v format-patch --stdout --no-signature --subject-prefix="RFC PATCH" -2
    v format-patch --stdout --signature="custom sig" -1
    v format-patch --stdout --no-signature --no-numbered -2
    v format-patch --stdout --no-signature --root HEAD~2
    v format-patch --no-signature HEAD~3
    cat 0*.patch >>"$TRANSCRIPT"
    rm -f 0*.patch
    v format-patch --no-signature -o out HEAD~3
    v format-patch --no-signature -o out/nested/deeper -2
    cat out/*.patch >>"$TRANSCRIPT"
    ls out out/nested/deeper >>"$TRANSCRIPT"
    v format-patch --root --no-signature -o all HEAD
    ls all >>"$TRANSCRIPT"
    v format-patch
    v format-patch --no-signature nosuch
}

s_format_patch_wrap() {
    v init -q repo; cd repo
    echo 0 >f; v add f; v commit -q -m base
    for n in $(seq 56 66); do
        tick; echo "$n" >>f
        v commit -q -a -m "$(printf 'w%.0s' $(seq 1 30)) $(printf 'x%.0s' $(seq 1 $((n - 31)))) tail words here"
    done
    v format-patch --stdout --no-signature -11 | grep -a -A2 '^Subject'
    tick; echo z >>f; v commit -q -a -m "$(printf 'y%.0s' $(seq 1 100)) short"
    v format-patch --stdout --no-signature -1 | grep -a -A2 '^Subject'
}

s_format_patch_options() {
    v init -q repo; cd repo
    echo 0 >f; v add f; v commit -q -m "[PATCH] fix: already tagged

body"
    tick; echo 1 >>f
    v commit -q -a -m "$(printf 'k%.0s' $(seq 1 40)) $(printf 'z%.0s' $(seq 1 50)) tail"
    tick; echo 2 >>f; v commit -q -a -m "plain subject"
    v format-patch --stdout --no-signature -3
    v format-patch --stdout --no-signature -k -3
    v format-patch --stdout --no-signature --keep-subject --start-number 5 -2
    v format-patch --stdout --no-signature -k -1 HEAD~2
    v format-patch --no-signature -k -o kout -3
    ls kout >>"$TRANSCRIPT"
    cat kout/* >>"$TRANSCRIPT"
    v format-patch --stdout -k --subject-prefix=RFC -1
    v format-patch --stdout -k -n -1
    v format-patch --stdout --directory=out -1
    v format-patch --directory=out -1
    ls >>"$TRANSCRIPT"
}

s_am() {
    v init -q repo; cd repo
    seq 1 10 >a; echo x >b; v add .; v commit -q -m base
    v branch other
    tick; sed -i 's/^3$/three/' a; v commit -q -a -m "Change three

Longer body.

Signed-off-by: Someone <s@x>"
    tick; echo more >>b; GIT_AUTHOR_NAME="Other Author" GIT_AUTHOR_EMAIL=oa@x v commit -q -a -m "Edit b"
    tick; echo n >n; mkdir sub; echo s >sub/s; v add n sub; v commit -q -m "Add n and sub"
    tick; git rm -q -f b; printf 'nonl' >nl; git add nl; chmod +x n; v commit -q -a -m "Remove b, add nl, chmod n"
    cd ..; g -C repo format-patch --no-signature -o ../patches -4 >/dev/null; cd repo
    cp ../patches/0001-Change-three.patch ../patches/c1.patch
    g -C repo format-patch --no-signature -o ../p1 -1 >/dev/null 2>&1
    v switch -q other
    tick; sed -i 's/^3$/THREE/' a; v commit -q -a -m "Conflicting three"
    v am ../patches/0001-Change-three.patch
    v status
    v status --short
    cat .git/ORIG_HEAD
    v am ../patches/0002-Edit-b.patch
    v am --continue
    v am --skip
    v log --oneline
    v am ../patches/0002-Edit-b.patch ../patches/0003-Add-n-and-sub.patch ../patches/0004-Remove-b-add-nl-chmod-n.patch
    v log --oneline
    v log -3 --format='%an <%ae> %ad | %cn | %s'
    v reflog
    v show --stat HEAD~1
    ls -l n | cut -c1-10
    cat nl; echo
    v am --abort
    v am --skip
    v am --continue
    v reset -q --hard "$(g rev-parse HEAD~3)"
    v am ../patches/0001-Change-three.patch
    v am --abort
    v log --oneline
    v reflog
    v status --short
    cat .git/ORIG_HEAD
    v reset -q --hard "$(g rev-parse HEAD~0)"
    v am ../patches/0001-Change-three.patch
    printf '1\n2\nTHREE\n4\n5\n6\n7\n8\n9\n10\n' >a; sed -i 's/THREE/three/' a
    v add a
    v am --continue
    v log --oneline
    v am nosuch.mbox
    echo x >>a; v am ../patches/0002-Edit-b.patch
    git checkout -q -- a
    v am ../patches/0002-Edit-b.patch ../patches/0001-Change-three.patch
    v status --short
    v am --skip
    v log --oneline
    # an unborn branch
    cd ..; v init -q fresh; cd fresh
    v am ../patches/0001-Change-three.patch
    v am --abort
    v am ../patches/0003-Add-n-and-sub.patch
    v status --short
    v log --oneline
    v reflog
}

s_notes_basic() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    tick; echo b >b; v add b; v commit -q -m two
    tick; echo c >c; v add c; v commit -q -m three
    v notes list
    v notes show HEAD
    v notes add -m 'hello' HEAD
    tick; v notes add -m 'again' HEAD
    tick; v notes add -f -m 'forced' HEAD
    tick; v notes append -m 'second para' HEAD
    tick; v notes append -m 'only para' HEAD~1
    v notes show HEAD
    v notes show HEAD~1
    v notes show HEAD~2
    v notes list
    v notes list HEAD
    v notes list HEAD~2
    tick; v notes copy HEAD HEAD~2
    tick; v notes copy HEAD~1 HEAD
    tick; v notes copy -f HEAD~1 HEAD
    v notes copy HEAD~2 HEAD~0
    v notes copy nosuch HEAD
    printf 'from file\n\n\n  indented  \n\n' >../msg.txt
    tick; v notes add -F ../msg.txt HEAD~2 -f
    v notes show HEAD~2
    tick; v notes add -f -m one -m two -m 'three   ' HEAD
    v notes show HEAD
    tick; v notes append -F ../msg.txt HEAD
    v notes show HEAD
    tick; v notes add -f -m '' HEAD~1
    v notes list
    v notes remove HEAD~1
    tick; v notes remove HEAD HEAD~2
    v notes remove HEAD
    v notes remove --ignore-missing HEAD
    v notes list
    tick; v notes add --allow-empty -m '' HEAD
    v notes show HEAD
    v notes show nosuch
    v notes add -m x nosuch
    v notes list nosuch
    tick; v notes --ref=extra add -m side HEAD
    v notes --ref=refs/notes/extra list
    v notes --ref=notes/other add -m other HEAD~1
    tick; git config core.notesRef refs/notes/viaconfig
    v notes add -m cfg HEAD
    v notes list
    v notes show HEAD
    git config --unset core.notesRef
    v notes list
    v log --format='%h %s' refs/notes/commits
    v log --oneline refs/notes/extra
    v reflog show refs/notes/commits
    v cat-file -p "$(g rev-parse refs/notes/commits^{tree})"
    v cat-file -p "$(g rev-parse refs/notes/viaconfig^{tree})"
}

# Notes on blobs, so one tree can be filled with hundreds of notes cheaply; `g notes copy --stdin`
# (real git, in both twins) builds the starting tree, and the rivet/git operation under test then
# rewrites it, which is where the fan-out has to come out identical.
notes_fill() {
    local count=$1
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    mkdir ../blobs; seq 1 "$count" | split -l 1 -a 6 - ../blobs/f
    g hash-object -w --stdin-paths < <(printf '%s\n' ../blobs/f*) >../oids
    g notes add -m seed HEAD
    paste -d' ' <(yes HEAD | head -n "$count") ../oids | g notes copy --stdin
}

s_notes_fanout() {
    notes_fill 600
    tick; v notes add -m extra "$(echo extra | g hash-object -w --stdin)"
    tick; v notes remove "$(sed -n 7p ../oids)"
    tick; v notes append -m more "$(sed -n 9p ../oids)"
    v notes list "$(sed -n 11p ../oids)"
    v cat-file -p "$(g rev-parse refs/notes/commits^{tree})"
}

s_notes_fanout_small() {
    notes_fill 255
    tick; v notes add -m extra "$(echo extra | g hash-object -w --stdin)"
    # Shrinking *below* the fan-out threshold is a known gap (git keeps untouched fan-out
    # subtrees verbatim); a handful of removals stays above it.
    for n in $(seq 2 20); do v notes remove "$(sed -n ${n}p ../oids)" >/dev/null; tick; done
    v cat-file -p "$(g rev-parse refs/notes/commits^{tree})"
}

s_notes_fanout_deep() {
    # 70000 notes in a debug build: the default 60 seconds per command is too tight when the suite
    # runs in parallel with the cargo tests.
    local V_TIMEOUT=300
    notes_fill 70000
    tick; v notes add -m extra "$(echo extra | g hash-object -w --stdin)"
    v cat-file -p "$(g rev-parse refs/notes/commits^{tree})"
}

# `Auto-merging <path>` names every file both sides changed, clean or not, and `-q` does not
# silence it (git 2.56: stdout is exactly that line). Files only one side touched are silent.
s_merge_auto_merging() {
    v init -q repo; cd repo
    printf '1\n2\n3\n4\n5\n' >f; printf 'x\ny\nz\n' >h; v add f h; v commit -q -m base
    v branch topic; v branch other
    tick; printf 'one\n2\n3\n4\n5\n' >f; echo m >only_main; v add only_main; v commit -q -a -m "main f"
    v switch -q topic
    tick; printf '1\n2\n3\n4\nfive\n' >f; echo t >new; printf 'x\ny\nZ\n' >h; v add new; v commit -q -a -m "topic f h new"
    v switch -q other
    tick; printf 'x\ny\nzz\n' >h; v commit -q -a -m "other h"
    v switch -q main
    v merge -q topic -m quiet
    v log --oneline
    v show --stat HEAD
    v merge other -m loud
    v log --oneline
}

# From a subdirectory `status` (long and --short) prints cwd-relative paths, `../` for paths
# outside it; --porcelain stays root-relative. Renames are relativised per side.
s_status_subdir() {
    v init -q repo; cd repo
    mkdir -p sub/deep other
    echo a >sub/a; echo b >other/b; echo r >root; echo d >sub/deep/d; echo q >sub/gone
    v add .; v commit -q -m base
    echo m >>sub/a; echo m >>other/b; echo n >sub/new; echo n >other/new; echo x >>root
    mkdir sub/newdir; echo z >sub/newdir/z
    rm sub/gone
    v mv sub/deep/d sub/deep/e
    echo s >sub/staged; v add sub/staged
    ( cd sub
      v status -s
      v status
      v status -s -uall
      v status -s -uno
      v status --porcelain
      v status -s -b
      cd deep
      v status -s
      v status )
    ( cd other; v status -s )
}

# `add <path>` after the file was deleted during a conflict stages the removal: every stage of the
# path goes, exit 0 (git 2.56). Other conflicted paths keep their stages.
s_add_deleted_unmerged() {
    v init -q repo; cd repo
    printf '1\n2\n3\n' >f; echo k >k; echo g >g; echo h >h; v add .; v commit -q -m base
    v branch topic
    tick; printf '1\nm\n3\n' >f; echo m >k; v rm -q g; echo m >h; v commit -q -a -m main
    v switch -q topic
    tick; printf '1\nt\n3\n' >f; v rm -q k; echo t >g; v rm -q h; v commit -q -a -m topic
    v switch -q main
    v merge topic
    v ls-files -s
    rm f
    v add f
    v ls-files -s
    v status -s
    rm h
    v add h
    v ls-files -s
    v add nosuchfile
    rm k
    v add k
    v add g
    v ls-files -s
    v status -s
    v commit -q -m resolved
    v ls-tree -r HEAD
    v status -s
}

# `<ref>@{N}`, `@{N}` and `@{-N}` select reflog entries; asking past the end is a fatal error that
# names the ref as typed (git 2.56: "log for 'topic' only has 3 entries", exit 128).
s_reflog_selector() {
    v init -q repo; cd repo
    echo 1 >f; v add f; tick; v commit -q -m c1
    echo 2 >f; tick; v commit -q -a -m c2
    v switch -q -c topic
    echo 3 >f; tick; v commit -q -a -m c3
    echo 4 >f; tick; v commit -q -a -m c4
    v switch -q main
    for spec in topic@{0} topic@{1} topic@{2} topic@{3} topic@{4} topic@{9} refs/heads/topic@{1} \
                heads/topic@{9} main@{0} main@{1} main@{2} HEAD@{0} HEAD@{3} HEAD@{99} @{0} @{1} @{99} \
                @{-1} @{-2} @{-1}@{1} topic@{01} topic@{1}~1 topic@{1}^{tree} topic@{1}^{commit} \
                nosuch@{1} topic@{x} topic@{-1} topic@{1}@{1} main@{} @{}; do
        v rev-parse "$spec"
    done
    v log -1 --format='%h %s' topic@{2}
    v log --oneline topic@{1}..topic
    v show -s --format=%s topic@{1}
    v diff --stat topic@{3} topic@{0}
    v branch from-reflog topic@{2}
    v branch bad-reflog topic@{9}
    v switch -c x topic@{2}
    v switch -c y topic@{9}
    v switch main
    v reset --hard topic@{1}
    v log --oneline
    v reset --hard topic@{9}
    v reset --hard @{1}
    v log --oneline
    v reflog
    # One past the oldest entry yields that entry's *old* id when it is not the null id (a log
    # whose head was expired away); with a null old id it is the error above.
    sed -i "1s/^0\\{40\\}/$(g rev-parse main)/" .git/logs/refs/heads/topic
    v rev-parse topic@{3} topic@{4} topic@{5}
}

# `HEAD detached at <name>` uses the name the detaching checkout was given (branches keep
# `refs/heads/`, tags and remote branches lose their namespace, other specs fall back to the short
# id) and becomes `detached from` once HEAD has moved on (git 2.56).
s_status_detached_name() {
    v init -q repo; cd repo
    echo 1 >f; v add f; tick; v commit -q -m c1
    echo 2 >f; tick; v commit -q -a -m c2
    v branch topic; v tag light; v tag -a -m ann annot
    g update-ref refs/remotes/origin/side HEAD
    for spec in topic refs/heads/topic heads/topic light annot origin/side main~1 'topic^0' HEAD \
                "$(g rev-parse --short HEAD)" "$(g rev-parse HEAD)" '@{-1}'; do
        v switch -q main
        v switch -q --detach "$spec"
        echo "== $spec"
        v status
        v status -sb
        v branch
    done
    v switch -q main
    v checkout -q topic
    v checkout -q --detach topic
    echo 3 >f; tick; v commit -q -a -m c3
    v status
    v branch
    v reset -q --hard topic
    v status
    v switch -q --detach annot
    tick; v commit -q --allow-empty -m empty
    v status
}

# Abbreviated ids of every length from 4 up resolve, odd lengths included (git's default abbrev is
# 7). A prefix that matches nothing is "unknown revision".
s_abbrev_odd_lengths() {
    v init -q repo; cd repo
    echo 1 >f; v add f; tick; v commit -q -m c1
    echo 2 >f; tick; v commit -q -a -m c2
    local full; full=$(g rev-parse HEAD)
    for n in 4 5 6 7 8 9 10 11 12 39 40; do
        v rev-parse "${full:0:$n}"
        v cat-file -t "${full:0:$n}"
    done
    v rev-parse "$(echo "${full:0:7}" | tr a-f A-F)"
    v rev-parse --verify "${full:0:7}^{tree}"
    v log -1 --format=%s "${full:0:7}"
    v rev-parse 0000000 1234567x
}

# Creating an annotated tag whose target is itself a tag prints git's nested-tag hint (stderr,
# naming the spec as typed); lightweight tags and commit targets stay silent; advice.nestedTag
# turns it off. (The brief's "warning: tag ... is actually an annotated tag" is not what git 2.56 prints.)
s_tag_nested_hint() {
    v init -q repo; cd repo
    echo 1 >f; v add f; tick; v commit -q -m c1
    tick; v tag -a -m first ann
    tick; v tag -a -m second nested ann
    tick; v tag -m third viam ann
    tick; v tag light ann
    tick; v tag -a -m onc oncommit HEAD
    tick; v tag -a -m peeled peeled 'ann^{}'
    tick; v tag -a -m full fullspec refs/tags/ann
    tick; v tag -a -m byid byid "$(g rev-parse ann)"
    tick; v tag -a -m deeper deeper nested
    tick; v tag -f -a -m again nested ann
    tick; v tag -a -m dup nested ann
    git config advice.nestedTag false
    tick; v tag -a -m viaconfig viaconfig ann
    git config advice.nestedTag true
    tick; v tag -a -m back back ann
    v tag -l
    v cat-file -p nested
    v cat-file -p deeper
}

# Real git writes a TREE (cached tree) extension on `write-tree`/`commit`; a rivet edit of the index
# must not leave it behind, or the next `git write-tree`/`git commit` reuses a stale tree id.
# After each rivet index edit git itself reads the index back: `write-tree`, `diff --cached` and a
# commit whose tree has to hold the edit.
s_index_cache_tree() {
    v init -q repo; cd repo
    mkdir -p d/e other; echo a >d/e/a; echo b >d/b; echo c >c; echo o >other/o; echo p >other/p
    v add .; tick; v commit -q -m base
    readback() {
        vg write-tree
        vg diff --cached --stat
        vg ls-files -s
    }
    fresh() { g write-tree >/dev/null; }

    fresh; echo A >d/e/a; v add d/e/a; readback
    fresh; echo n >d/e/new; v add d/e/new; readback
    fresh; v rm -q d/b; readback
    fresh; v mv c d/e/c; readback
    fresh; v restore --staged d/e/a; readback
    fresh; v reset -q -- d/e/new; readback
    fresh; v add d/e/new; fresh; v rm -q --cached d/e/new; readback
    fresh; echo Z >other/o; v add other; readback
    fresh; v checkout HEAD -- d/e/a; readback
    echo changed >other/p; g diff other/p >../cached.patch; g checkout -q -- other/p
    fresh; v apply --cached ../cached.patch; readback
    tick; vg commit -q -m "after rivet edits"
    vg ls-tree -r HEAD
    vg fsck --strict
    v status -s
}

# `switch --merge` / `checkout -m` (git 2.56): when the plain switch would overwrite tracked local
# changes, they are stashed ("autostash while switching to '<target>'"), the switch happens, and the
# stash is applied on top with markers labelled `<target>`/`local`: clean -> "Applied autostash." and
# the entry is dropped (changes unstaged); conflicts -> the entry stays, exit 0, stages in the index.
switch_merge_repo() {
    v init -q repo; cd repo
    printf '1\n2\n3\n4\n5\n6\n7\n8\n9\n' >f; echo g >g; echo h >h
    v add .; tick; v commit -q -m base
    v branch topic; v branch same
    printf '1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\n' >f; echo gm >g
    tick; v commit -q -a -m main
    v switch -q same; tick; v commit -q --allow-empty -m same2
    v switch -q topic
    printf '1\nONE-topic\n3\n4\n5\n6\n7\n8\n9\n' >f; echo gt >g; echo ht >h
    tick; v commit -q -a -m topic
    v switch -q main
}

switch_merge_check() {
    vg status -s
    vg stash list
    vg ls-files -s
    vg diff --stat
}

s_switch_merge() {
    switch_merge_repo
    # no local changes: nothing to stash
    tick; v switch -m topic
    v switch -q main
    # plain switch is possible: no stash even with -m
    echo more >>f; tick; v switch -m same; switch_merge_check
    v reset -q --hard; v switch -q main
    # local edit merges cleanly: Applied autostash, changes come back unstaged
    printf 'zero\n1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\n' >f; tick; v switch -m topic
    switch_merge_check; cat f
    v reset -q --hard; v switch -q main
    # conflicting edit: stash kept, markers, stages
    printf '1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\nlocal-tail\n' >f; tick; v switch -m topic
    switch_merge_check; cat f
    vg cat-file -p 'stash@{0}'
    vg cat-file -p 'stash@{0}^2'
    vg reflog show stash
    vg reflog -3
    # resolve by hand, finish, drop the stash
    printf 'resolved\n' >f; v add f; tick; v commit -q -m resolved; v stash drop
    v switch -q main
}

s_switch_merge_states() {
    switch_merge_repo
    # staged + modified + untracked: the staged edit conflicts, the untracked file stays
    printf '1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\nB\n' >f; v add f; echo g2 >>g; echo u >untracked
    tick; v switch -m topic; switch_merge_check; cat f g
    v reset -q --hard; rm -f untracked; v stash clear; v switch -q main
    # added, deleted and conflicting paths together
    printf '1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\nlocal-tail\n' >f; echo new >newf; v add newf; v rm -q -f h; echo more >>g
    tick; v switch -m topic; switch_merge_check
    v reset -q --hard; v stash clear; v switch -q main
    # a merge that is clean for f but conflicts for h: the clean part is staged
    echo new >newf; v add newf; v rm -q -f h; printf 'zero\n1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\n' >f
    tick; v switch -m topic; switch_merge_check
    v reset -q --hard; v stash clear; v switch -q main
    # -q keeps the autostash notices
    printf '1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\nlocal-tail\n' >f; tick; v switch -q -m topic; switch_merge_check
    v reset -q --hard; v stash clear; v switch -q main
    printf 'zero\n1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\n' >f; tick; v switch -q -m topic; switch_merge_check
    v reset -q --hard; v switch -q main
}

s_switch_merge_detached() {
    switch_merge_repo
    printf '1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\nlocal-tail\n' >f; tick; v switch -m --detach topic
    switch_merge_check; cat f
    v reset -q --hard; v stash clear; v switch -q main
    # from a detached HEAD
    v switch -q --detach main
    printf '1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\nlocal-tail\n' >f; tick; v switch -m topic
    switch_merge_check; cat f; v status
    v reset -q --hard; v stash clear; v switch -q main
    printf 'zero\n1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\n' >f; tick; v switch -m --detach topic; switch_merge_check
}

s_checkout_merge() {
    switch_merge_repo
    printf '1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\nlocal-tail\n' >f; tick; v checkout -m topic
    switch_merge_check; cat f
    v reset -q --hard; v stash clear; v checkout -q main
    printf 'zero\n1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\n' >f; tick; v checkout -m topic; switch_merge_check
    v reset -q --hard; v checkout -q main
    printf 'zero\n1\n2\n3\n4\n5\n6\n7\n8\nNINE-main\n' >f; tick; v checkout --merge --detach topic; switch_merge_check
}

s_switch_merge_refused() {
    switch_merge_repo
    # an untracked file in the way is refused exactly like a plain switch; nothing is stashed
    echo hi >newfile; v add newfile; tick; v commit -q -m addnew; v switch -q topic
    echo collide >newfile
    v switch -m main; switch_merge_check
    rm newfile
    # unknown targets fail like a plain switch
    v switch -q main; echo x >>f
    v switch -m nosuch; v switch -m topic~0; switch_merge_check
    # --merge together with --create / --conflict is not implemented here (git supports them)
}

# A merge-heavy history with several authors, a skewed clock and author dates unlike committer
# dates, shared by the rev-list scenarios below.
rl_t=1700000000
rl_commit() {
    rl_t=$((rl_t + ${4:-60}))
    GIT_AUTHOR_NAME="$1" GIT_AUTHOR_EMAIL="$2" GIT_AUTHOR_DATE="$((rl_t - ${5:-0})) +0000" \
        GIT_COMMITTER_NAME=C GIT_COMMITTER_EMAIL=c@e GIT_COMMITTER_DATE="$rl_t +0000" \
        v commit -q --allow-empty -m "$3"
}
rl_merge() {
    rl_t=$((rl_t + 60))
    GIT_AUTHOR_NAME=Merger GIT_AUTHOR_EMAIL=m@e GIT_AUTHOR_DATE="$rl_t +0000" \
        GIT_COMMITTER_NAME=C GIT_COMMITTER_EMAIL=c@e GIT_COMMITTER_DATE="$rl_t +0000" \
        v merge -q --no-ff -m "$2" "$1"
}
rl_repo() {
    rl_t=1700000000
    v init -q repo; cd repo
    rl_commit Alice alice@x.org "root: first"
    rl_commit Bob bob@x.org "second line fix" 60 30
    v switch -q -c side1; rl_commit Carol carol@y.com "side1 a"; rl_commit Carol carol@y.com "side1 b: Fixes #12"
    v switch -q main; rl_commit Alice alice@x.org "main c"
    v switch -q -c side2; rl_commit Bob bob@x.org "side2 a" 5 900; rl_commit Dave dave@z.net "side2 b" 5
    v switch -q main; rl_merge side1 "Merge side1"; rl_commit Alice alice@x.org "main d: fix typo"
    rl_merge side2 "Merge side2"
    v switch -q -c side3 main~2; rl_commit Erin erin@y.com "side3 a (skewed clock)" -5000; rl_commit Erin erin@y.com "side3 b" 10
    v switch -q main; rl_commit Alice alice@x.org "main e"; rl_merge side3 "Merge side3"
    rl_commit Bob bob@x.org "final: FIX everything
Body line one
second body line: fixes" 
    v tag v1 main~3; v tag -a -m rel v2 main~1
}

s_rev_list_filters() {
    rl_repo
    v rev-list main
    v rev-list --all
    v rev-list --count main
    v rev-list --parents main
    v rev-list --parents main~3..main
    v rev-list --parents --first-parent main
    v rev-list --parents ^v1 main side3
    v rev-list --no-merges main
    v rev-list --merges main
    v rev-list --min-parents=2 main
    v rev-list --max-parents=1 main
    v rev-list --max-parents=0 main
    v rev-list --min-parents=1 --max-parents=1 main
    v rev-list --no-merges --count main
    v rev-list --since=1700000200 main
    v rev-list --until=1700000300 main
    v rev-list --since=1700000200 --until=1700000400 main
    v rev-list --after=1700000200 --before=1700000400 main
    v rev-list --since='2023-11-14 22:16:40 +0000' main
    v rev-list --until='2023-11-14 22:18:20 +0000' main
    v rev-list --since='2030-01-01 00:00:00 +0000' main
    v rev-list --since='1700000200 +0100' main
    v rev-list --author=Alice main
    v rev-list --author='<bob@' main
    v rev-list --author=Alice --author=Bob main
    v rev-list --author=alice@x.org main
    v rev-list --author='x.org>' main
    v rev-list --author=1700 main
    v rev-list --author=nobody main
    v rev-list --committer=C main
    v rev-list --committer=Alice main
    v rev-list --grep=fix main
    v rev-list --grep=Fixes --grep=typo main
    v rev-list --grep=fix -i main
    v rev-list --grep=fix --regexp-ignore-case main
    v rev-list --grep=FIX --invert-grep main
    v rev-list --grep=fix --grep=typo --all-match main
    v rev-list --grep='fix(es)?' -E main
    v rev-list --grep='fix\(es\)\?' main
    v rev-list --grep='^Body' main
    v rev-list --grep='Fixes #[0-9]' main
    v rev-list --grep=zzz main
    v rev-list --author=Alice --grep=fix main
    v rev-list --author=Bob --grep=fix --no-merges main
    v rev-list --author=Alice --since=1700000200 --parents main
    v rev-list --author=Alice --max-count=2 main
    v rev-list --grep=fix --reverse main
    v rev-list --grep=fix --count main
    v rev-list -F --grep='main d' main
    v rev-list --fixed-strings --grep='fix (' main
    v rev-list --skip=2 main
    v rev-list --skip 2 main
    v rev-list --skip=2 -n 2 main
    v rev-list --skip=2 --count main
    v rev-list --skip=1 --reverse main
    v rev-list --skip=1 -n 2 --reverse main
    v rev-list --skip=0 -n 0 main
    v rev-list --skip=-1 main
    v rev-list --skip=999 main
    v rev-list --skip=1 --no-merges main
    v rev-list --skip=x main
    v rev-list --max-age=1700000200 main
    v rev-list --min-age=1700000300 main
    v rev-list --max-age=1700000200 --min-age=1700000400 main
    v rev-list --max-age=1700000300 --since=1700000100 main
    v rev-list --since=1700000300 --max-age=1700000100 main
    v rev-list --max-age=abc main
}

s_rev_list_left_right() {
    rl_repo
    v rev-list --left-right main...side3
    v rev-list --left-right side3...main
    v rev-list --left-right main...side1
    v rev-list --left-right main
    v rev-list --left-right side3..main
    v rev-list --left-right --parents main...side3
    v rev-list --left-right --boundary main...side3
    v rev-list --left-right --abbrev-commit main...side3
    v rev-list --left-right --reverse main...side3
    v rev-list --left-right --first-parent main...side3
    v rev-list --left-right -n 2 main...side3
    v rev-list --left-right --skip=1 main...side3
    v rev-list --left-right --no-merges main...side3
    v rev-list --left-right --count main...side3
    v rev-list --left-right --count main
    v rev-list --left-right --count side3..main
    v rev-list --left-right --boundary --count main...side3
    v rev-list --left-right --count --objects main...side3
    v rev-list --left-only main...side3
    v rev-list --right-only main...side3
    v rev-list --left-only main
    v rev-list --right-only main
    v rev-list --left-only --count main...side3
    v rev-list --right-only --count main...side3
    v rev-list --left-right --left-only main...side3
    v rev-list --left-right --left-only --count main...side3
    v rev-list --left-only --right-only main...side3
}

s_rev_list_ancestry_path() {
    rl_repo
    v rev-list --ancestry-path main~6..main
    v rev-list --ancestry-path side1..main
    v rev-list --ancestry-path side2..main
    v rev-list --ancestry-path ^side1 main
    v rev-list --ancestry-path main~3..main
    v rev-list --ancestry-path v1..main
    v rev-list --ancestry-path main...side3
    v rev-list --ancestry-path --parents side1..main
    v rev-list --ancestry-path --first-parent side1..main
    v rev-list --ancestry-path --reverse side1..main
    v rev-list --ancestry-path --count side1..main
    v rev-list --ancestry-path --boundary side1..main
    v rev-list --ancestry-path -n 2 side1..main
    v rev-list --ancestry-path --skip=1 side1..main
    v rev-list --ancestry-path main
    v rev-list --ancestry-path main..main
}

s_rev_list_ancestry_path_commit() {
    rl_repo
    v rev-list --ancestry-path=side1 main
    v rev-list --ancestry-path=side2 main
    v rev-list --ancestry-path=v1 main
    v rev-list --ancestry-path=main~5 main~8..main
    v rev-list --ancestry-path=side1 side2..main
    v rev-list --ancestry-path=side1 --ancestry-path=side2 main
    v rev-list --ancestry-path=v1 --parents main
    v rev-list --ancestry-path=side1 --reverse main
    v rev-list --ancestry-path=side1 --count main
    v rev-list --ancestry-path=side1 --boundary main~4..main
    v rev-list --ancestry-path=nonexistent main
    v rev-list --ancestry-path=v1 --ancestry-path main
}

s_rev_list_ancestry_path_skew() {
    v init -q repo; cd repo
    sk() { # name date [parent...]: an empty commit at base+date on the first parent, tagged name
        local n=$1 d=$((1700000000 + $2)); shift 2
        if [ $# -eq 0 ]; then v switch -q --orphan "o-$n"; else v switch -q --detach "$1"; fi
        GIT_AUTHOR_NAME=A GIT_AUTHOR_EMAIL=a@e GIT_AUTHOR_DATE="$d +0000" \
            GIT_COMMITTER_NAME=C GIT_COMMITTER_EMAIL=c@e GIT_COMMITTER_DATE="$d +0000" \
            v commit -q --allow-empty -m "$n"
        if [ $# -gt 1 ]; then
            GIT_AUTHOR_NAME=A GIT_AUTHOR_EMAIL=a@e GIT_AUTHOR_DATE="$d +0000" \
                GIT_COMMITTER_NAME=C GIT_COMMITTER_EMAIL=c@e GIT_COMMITTER_DATE="$d +0000" \
                v merge -q --no-ff -m "$n" "$2"
        fi
        v tag "$n"
    }
    # X=s5 is reached through s3 and s1; s1 (500) is walked before its child s3 (300), so the
    # flag that keeps X's ancestors stops there and the root s0 (1000) is dropped
    sk s0 1000
    sk s1 500 s0
    sk s2 900 s1
    sk s3 300 s1
    sk s4 400 s2
    sk s5 300 s3
    sk s6 500 s5 s4
    v rev-list --ancestry-path=s5 s6
    v rev-list --ancestry-path=s5 --parents s6
    v rev-list --ancestry-path=s5 --count s6
    v rev-list --ancestry-path=s5 --reverse s6
    v rev-list --ancestry-path=s3 s6
    v rev-list --ancestry-path=s1 s6
    v rev-list --ancestry-path=s2 s6
    v rev-list --ancestry-path=s4 s6
    v rev-list --ancestry-path=s5 s4
    v rev-list --ancestry-path=s5 s5
    v rev-list --ancestry-path=s5 s1..s6
    v rev-list --ancestry-path=s5 s2..s6
    # a bottom that the walk never reaches lists nothing, not even its ancestors
    v rev-list --ancestry-path=s4 s5
    v rev-list --ancestry-path=s2 s3
    # the root is dated before the others: it is dropped all the same
    sk t0 100
    sk t1 500 t0
    sk t2 900 t1
    sk t3 300 t1
    sk t4 400 t2
    sk t5 300 t3
    sk t6 500 t5 t4
    v rev-list --ancestry-path=t5 t6
    v rev-list --ancestry-path=t3 t6
    # a merge older than both parents
    sk u0 400
    sk u1 900 u0
    sk u2 600 u1
    sk ua 700 u1
    sk u7 200 u2 ua
    v rev-list --ancestry-path=u2 u7
    v rev-list --ancestry-path=u2 --boundary u7
}

s_rev_list_cherry() {
    v init -q repo; cd repo
    tick; echo 0 >f; v add f; v commit -q -m base
    v switch -q -c side
    for i in a b c; do tick; echo $i >$i; v add $i; v commit -q -m side-$i; done
    v switch -q main
    tick; echo m >m; v add m; v commit -q -m main-m
    # side-b lands on main too, so both sides of main...side hold an equivalent patch
    tick; v cherry-pick side~1 >/dev/null
    tick; echo z >z; v add z; v commit -q -m main-z
    v rev-list --cherry-pick main...side
    v rev-list --cherry-pick --left-right main...side
    v rev-list --cherry-mark main...side
    v rev-list --cherry-mark --left-right main...side
    v rev-list --cherry-mark --left-right --count main...side
    v rev-list --cherry-mark --count main...side
    v rev-list --cherry-pick --count main...side
    v rev-list --cherry-pick --left-right --count main...side
    v rev-list --cherry-pick --right-only main...side
    v rev-list --cherry-mark --right-only main...side
    v rev-list --cherry-mark --left-only main...side
    v rev-list --cherry-mark --left-right --count --right-only main...side
    v rev-list --cherry-pick --boundary main...side
    v rev-list --cherry-mark --boundary --left-right main...side
    v rev-list --cherry-mark --parents main...side
    v rev-list --cherry-mark --reverse main...side
    v rev-list --cherry-mark -n 3 main...side
    v rev-list --cherry-pick --skip=1 main...side
    v rev-list --cherry-mark main..side
    v rev-list --cherry-pick main..side
    v rev-list --cherry-pick main
    v rev-list --cherry-mark main
    v rev-list --cherry-pick --cherry-mark main...side
    # a merge has no patch id and is never equivalent to anything
    v switch -q -c merged side
    tick; v merge -q --no-ff -m merge-main main^
    v rev-list --cherry-mark --left-right merged...main
    v rev-list --cherry-pick merged...main
}

s_rev_list_children() {
    rl_repo
    v rev-list --children main
    v rev-list --children --all
    v rev-list --children main side3 side1
    v rev-list --children side1..main
    v rev-list --children --reverse main
    v rev-list --children --first-parent main
    v rev-list --children --boundary main~3..main
    v rev-list --children --date-order --all
    v rev-list --children -n 3 main
    v rev-list --children --count main
    v rev-list --children --parents main
}

s_rev_list_abbrev() {
    rl_repo
    v rev-list --abbrev-commit main
    v rev-list --abbrev-commit --parents main
    v rev-list --abbrev=10 --abbrev-commit main
    v rev-list --abbrev=4 --abbrev-commit main
    v rev-list --abbrev-commit --no-abbrev-commit main
    v rev-list --abbrev=12 main
    v rev-list --abbrev-commit --abbrev=40 main
    v rev-list --abbrev-commit --boundary main~3..main
}

s_rev_list_order() {
    rl_repo
    for order in "" --date-order --topo-order --author-date-order; do
        v rev-list $order main
        v rev-list $order --all
        v rev-list $order --parents main
        v rev-list $order main side3 side1
        v rev-list $order ^v1 main
        v rev-list $order v1...main
        v rev-list $order side3..main
        v rev-list $order --reverse main
        v rev-list $order -n4 main
        v rev-list $order --first-parent main
        v rev-list $order --no-merges main
        v rev-list $order --since=1700000200 main
        v rev-list $order --author=Alice main
        v rev-list $order side2 side1 main~4
    done
}

s_rev_list_boundary() {
    rl_repo
    v rev-list --boundary main~3..main
    v rev-list --boundary v1..main
    v rev-list --boundary --parents v1..main
    v rev-list --boundary --topo-order v1..main
    v rev-list --boundary --date-order v1..main
    v rev-list --boundary side3..main
    v rev-list --boundary main..side3
    v rev-list --boundary ^side1 main side2
    v rev-list --boundary main~1..main
    v rev-list --boundary main
    v rev-list --boundary -n2 main
    v rev-list --boundary -n2 v1..main
    v rev-list --boundary --no-merges v1..main
    v rev-list --boundary --first-parent v1..main
    v rev-list --boundary --reverse v1..main
    v rev-list --boundary --count v1..main
    v rev-list --boundary --abbrev-commit v1..main
    v rev-list --boundary --author=Alice v1..main
    v rev-list --boundary v1...main
    v rev-list --boundary side1..side2
    v rev-list --boundary --all ^v1
}

# History with several authors and time zones, annotated/nested/blob/tree tags, remote-tracking refs and
# branch upstreams (ahead, behind, gone, local), built by real git in both twins so only the
# for-each-ref queries differ between the runs.
fer_t=1700000000
fer_commit() {
    fer_t=$((fer_t + ${5:-100}))
    GIT_AUTHOR_NAME="$1" GIT_AUTHOR_EMAIL="$2" GIT_AUTHOR_DATE="$fer_t $3" \
        GIT_COMMITTER_NAME=Comm GIT_COMMITTER_EMAIL=comm@e.org GIT_COMMITTER_DATE="$((fer_t + 7)) +0000" \
        g commit -q --allow-empty -m "$4"
}
fer_tag() { # name date tz message [object]
    GIT_COMMITTER_NAME=Tagger GIT_COMMITTER_EMAIL=tag@e.org GIT_COMMITTER_DATE="$2 $3" g tag -a -m "$4" "$1" ${5:-}
}
fer_repo() {
    fer_t=1700000000
    g init -q repo; cd repo
    fer_commit "Alice Ay" alice@x.org +0100 "root commit"
    fer_commit "Bob Bee" bob@y.net -0500 "second: a subject
that wraps onto a second line

Body paragraph one.
Body line two.

Signed-off-by: Bob Bee <bob@y.net>"
    g switch -q -c topic; fer_commit "Carol Cee" carol@z.com +0530 "topic work"; fer_commit "Alice Ay" alice@x.org +0100 "topic more"
    g switch -q main; fer_commit "Bob Bee" bob@y.net -0500 "main three"
    g switch -q -c feat main~1; fer_commit "Dave Dee" dave@w.io +0000 "feat only" 5
    g switch -q main
    fer_t=$((fer_t + 60))
    GIT_AUTHOR_NAME=Merger GIT_AUTHOR_EMAIL=m@e.org GIT_AUTHOR_DATE="$fer_t +0200" GIT_COMMITTER_NAME=Comm \
        GIT_COMMITTER_EMAIL=comm@e.org GIT_COMMITTER_DATE="$fer_t +0000" g merge -q --no-ff -m "Merge topic" topic
    fer_commit "Alice Ay" alice@x.org +0100 "main five"
    g tag light main~2
    fer_tag v1 1700000900 -0700 "Release one
second line of tag

third paragraph" main~1
    GIT_COMMITTER_NAME=Tagger2 GIT_COMMITTER_EMAIL=tag2@e.org GIT_COMMITTER_DATE="1700000950 +0900" g tag -a -m "tag of a tag" v1-wrap v1 2>/dev/null
    fer_tag v2 1700001000 +0000 "tag of topic" topic
    g update-ref refs/tags/blobtag "$(echo blob | g hash-object -w --stdin)"
    g update-ref refs/tags/treetag "$(g rev-parse 'main^{tree}')"
    g remote add origin https://example.invalid/r.git
    g update-ref refs/remotes/origin/main main~2
    g update-ref refs/remotes/origin/topic topic
    g update-ref refs/remotes/origin/feat feat^
    g config branch.main.remote origin; g config branch.main.merge refs/heads/main
    g config branch.topic.remote origin; g config branch.topic.merge refs/heads/topic
    g config branch.feat.remote origin; g config branch.feat.merge refs/heads/gone
    g branch loc main~3; g config branch.loc.remote .; g config branch.loc.merge refs/heads/feat
    g branch nouplink main~1
}

s_for_each_ref_atoms() {
    fer_repo
    v for-each-ref
    v for-each-ref --format='%(refname)|%(refname:short)|%(refname:lstrip=1)|%(refname:lstrip=2)|%(refname:rstrip=1)|%(refname:strip=2)|%(refname:lstrip=-1)|%(refname:rstrip=-1)|%(refname:rstrip=-2)'
    v for-each-ref --format='%(objectname) %(objectname:short) %(objectname:short=10) %(objecttype) %(objectsize)'
    v for-each-ref --format='%(refname) tree=%(tree) parent=%(parent) n=%(numparent)'
    v for-each-ref --format='%(refname) object=%(object) type=%(type) tag=%(tag)'
    v for-each-ref --format='%(refname) *%(*objectname) *%(*objecttype) *%(*objectsize) *%(*tree) *%(*authorname) *%(*subject) *%(*numparent)'
    v for-each-ref --format='%(refname) %(author)|%(committer)|%(tagger)|%(creator)'
    v for-each-ref --format='%(refname) %(authorname)|%(authoremail)|%(authoremail:trim)|%(authoremail:localpart)'
    v for-each-ref --format='%(refname) %(committername)|%(committeremail)|%(taggername)|%(taggeremail)'
    v for-each-ref --format='%(refname) %(*taggername) %(*committername) %(*authorname)' refs/tags
}

s_for_each_ref_dates() {
    fer_repo
    for style in default iso iso8601 iso-strict short raw unix rfc rfc2822; do
        v for-each-ref --format="%(refname:short) %(authordate:$style)|%(committerdate:$style)|%(taggerdate:$style)|%(creatordate:$style)"
    done
    v for-each-ref --format='%(refname:short) %(authordate)|%(committerdate)|%(taggerdate)|%(creatordate)'
    v for-each-ref --format='%(refname:short) %(authordate:format:%Y-%m-%d %H:%M:%S %z)|%(creatordate:format:%a %b %e %j %I%p %A %B %y %u %w %s %%)' refs/heads refs/tags/v1
    v for-each-ref --format='%(refname:short) %(*creatordate:iso)|%(*authordate:short)' refs/tags
    v for-each-ref --format='%(authordate:bogus)' refs/heads/main
}

s_for_each_ref_contents() {
    fer_repo
    v for-each-ref --format='[%(refname:short)] [%(subject)] [%(body)]'
    v for-each-ref --format='[%(refname:short)] [%(contents)]'
    v for-each-ref --format='[%(refname:short)] [%(contents:subject)] [%(contents:body)] [%(contents:signature)]'
    v for-each-ref --format='[%(refname:short)] [%(contents:lines=1)] [%(contents:lines=2)] [%(contents:lines=9)]'
    v for-each-ref --format='[%(refname:short)] [%(*subject)] [%(*body)] [%(*contents)]' refs/tags
    v for-each-ref --format='%(contents:bogus)' refs/heads/main
}

s_for_each_ref_quote() {
    fer_repo
    g tag -a -m "$(printf 'it'"'"'s [a] {b} $c "d" \\e!\tf\r\n\nbody')" quoted
    local q
    for q in --shell --perl --python --tcl -s -p; do
        v for-each-ref $q --format='%(refname:short)|%(objecttype)|%(contents)|%(taggername)|lit' refs/heads refs/tags
        v for-each-ref $q --format='%(HEAD) %(refname:short) %(objectname:short)' refs/heads
    done
    v for-each-ref --python --format='%(contents:subject)' refs/tags/quoted
    v for-each-ref --shell --format='%(if)%(taggername)%(then)Y %(refname)%(else)N %(refname)%(end)|lit' refs/tags refs/heads
    v for-each-ref --python --format='%(align:12)%(refname:short)%(end)|%(objecttype)' refs/heads
    v for-each-ref --shell --omit-empty --format='%(taggername)' refs/tags refs/heads
    v for-each-ref --shell --shell --format='%(refname)' refs/heads
    v for-each-ref --shell --no-shell --format='%(refname)' refs/heads
    v for-each-ref --shell --perl --format='%(refname)' refs/heads
    v for-each-ref -s --python --format='%(refname)' refs/heads
}

s_for_each_ref_stdin_start() {
    fer_repo
    local fmt='--format=%(refname)' first
    first=$(g for-each-ref --format='%(refname)' | sed -n 2p)
    v for-each-ref "$fmt" --start-after="$first"
    v for-each-ref "$fmt" --start-after=refs/heads/a
    v for-each-ref "$fmt" --start-after=refs/zzz
    v for-each-ref "$fmt" --start-after=
    v for-each-ref "$fmt" --start-after="$first" --count=1
    v for-each-ref "$fmt" --start-after="$first" --start-after=refs/heads/main
    v for-each-ref "$fmt" --start-after="$first" --no-start-after
    v for-each-ref "$fmt" --start-after="$first" --exclude=refs/tags
    v for-each-ref "$fmt" --start-after="$first" --sort=refname
    v for-each-ref "$fmt" --start-after="$first" --sort=-refname refs/heads
    v for-each-ref "$fmt" --start-after="$first" refs/heads
    printf 'refs/heads\n' >pat.txt
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin refs/tags
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin --start-after="$first"
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin --sort=-refname
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin --exclude=refs/heads/main
    printf 'refs/tags\nrefs/heads/m*\n' >pat.txt
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin
    printf 'refs/heads/main\r\n\nrefs/tags' >pat.txt
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin
    printf '\n' >pat.txt
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin
    : >pat.txt
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin
    STDIN_FILE=pat.txt v for-each-ref "$fmt" --stdin --start-after="$first"
}

s_for_each_ref_root_refs() {
    fer_repo
    local fmt='--format=%(refname)|%(refname:short)|%(objecttype)|%(symref)' oid
    oid=$(g rev-parse HEAD)
    v for-each-ref "$fmt"
    v for-each-ref --include-root-refs "$fmt"
    for n in ORIG_HEAD CHERRY_PICK_HEAD REBASE_HEAD A_HEAD AA_B_HEAD _HEAD FETCH_HEAD MERGE_HEAD HEAD2 HEAD_ HEADHEAD lower_HEAD Head X-HEAD FOO_ORIG STASH; do
        echo "$oid" >".git/$n"
    done
    mkdir .git/DIR_HEAD
    echo "ref: refs/heads/main" >.git/SYM_HEAD
    echo "ref: refs/heads/nonexistent" >.git/DANG_HEAD
    echo "$oid" >.git/refs/ROOTISH_HEAD
    v for-each-ref --include-root-refs "$fmt"
    v for-each-ref --include-root-refs --no-include-root-refs "$fmt"
    v for-each-ref --no-include-root-refs --include-root-refs "$fmt"
    v for-each-ref --include-root-refs "$fmt" HEAD 'SYM*'
    v for-each-ref --include-root-refs "$fmt" 'A*_HEAD'
    v for-each-ref --include-root-refs "$fmt" ORIG
    v for-each-ref --include-root-refs "$fmt" refs/heads
    v for-each-ref --include-root-refs "$fmt" --exclude='A*_HEAD' --exclude='ORIG*'
    v for-each-ref --include-root-refs "$fmt" --exclude=HEAD --count=3
    v for-each-ref --include-root-refs "$fmt" --sort=-refname --count=4
    v for-each-ref --include-root-refs "$fmt" --start-after=ORIG_HEAD
    v for-each-ref --include-root-refs --format='%(HEAD) %(refname) %(objectname:short) %(upstream)' HEAD
    v for-each-ref --include-root-refs --merged=HEAD "$fmt" 'ORIG*' HEAD refs/heads/main
    v for-each-ref --include-root-refs --no-contains=HEAD "$fmt"
    v for-each-ref --include-root-refs --contains=HEAD "$fmt"
    v for-each-ref --include-root-refs --points-at=HEAD "$fmt"
    printf 'HEAD\nrefs/tags\n' >pat.txt
    STDIN_FILE=pat.txt v for-each-ref --include-root-refs "$fmt" --stdin
    echo garbage >.git/BAD_HEAD
    v for-each-ref --include-root-refs "$fmt"
    v for-each-ref "$fmt"
    g pack-refs --all
    v for-each-ref --include-root-refs "$fmt"
}

s_for_each_ref_patterns() {
    fer_repo
    local fmt='--format=%(refname)' oid p
    oid=$(g rev-parse HEAD)
    for r in refs/heads/a refs/heads/b1 refs/heads/ab/c refs/heads/x/y/z refs/tags/v1 refs/tags/v10 refs/top; do
        g update-ref "$r" "$oid"
    done
    for p in 'refs/*' 'refs/heads*' 'refs/heads/*' 'refs/heads/?' 'refs/heads/[a-b]*' 'refs/heads/[!a]*' \
        'refs/heads/[^a]*' 'refs/*/a' 'refs/heads/**' 'refs/**/z' 'refs/**' '**/c' 'refs/heads/a*/c' \
        'refs/tags/v[0-9]' 'refs/tags/v?0' 'refs/tags/v[[:digit:]]*' 'refs/heads/a\*' 'refs/heads/[' \
        'refs/heads/a**' 'refs/heads/*/*' 'refs/*/*/*' 'refs/heads/x/**/z' 'refs/heads/x/*/z'; do
        v for-each-ref "$fmt" "$p"
    done
    v for-each-ref "$fmt" --exclude='refs/heads/*'
    v for-each-ref "$fmt" --exclude='refs/heads/[ab]*' refs/heads/
    v for-each-ref "$fmt" --exclude='refs/*' 'refs/**'
    v for-each-ref "$fmt" --exclude='refs/heads/?' --exclude='refs/tags/v[0-9]' refs/heads refs/tags
}

s_for_each_ref_version_sort() {
    fer_repo
    local fmt='--format=%(refname:short)' oid s
    oid=$(g rev-parse HEAD)
    for t in v1.9 v1.10 v1.2 v1.10-rc1 v1.10.1 v2 a1 a10 a2 a01 a001 V1.5 x-1 x-01 x1.0 x1.00 b007 b7; do
        g update-ref "refs/tags/z/$t" "$oid"
    done
    for s in version:refname v:refname -version:refname -v:refname version:refname:short v:refname:rstrip=1 \
        version:authordate version:objectname; do
        v for-each-ref "$fmt" --sort="$s" refs/tags/z
    done
    v for-each-ref "$fmt" --sort=refname --sort=version:refname refs/tags/z
    v for-each-ref "$fmt" --sort=version:refname --sort=-refname refs/tags/z
    v for-each-ref "$fmt" --sort=-v:refname --sort=objectname refs/tags/z
    v for-each-ref "$fmt" --sort=version:refname --ignore-case refs/tags/z
    v for-each-ref "$fmt" --sort=version:refname --count=3 refs/tags/z
    v for-each-ref "$fmt" --sort=version:
    v for-each-ref "$fmt" --sort=v:
    v for-each-ref "$fmt" --sort=version:nosuchatom
    v for-each-ref "$fmt" --sort=version:version:refname
}

s_for_each_ref_version_suffix() {
    fer_repo
    local fmt='--format=%(refname:short)' oid
    oid=$(g rev-parse HEAD)
    for t in v1.0 v1.0-rc1 v1.0-rc2 v1.0-rc10 v1.0-beta1 v1.0-rcx v1.0-foo v1.0.1 v1.1-beta v1.1 v0.9 v1.0-pre-rc1 v1.0-pre v1.0-rc1-pre; do
        g update-ref "refs/tags/z/$t" "$oid"
    done
    v for-each-ref "$fmt" --sort=version:refname refs/tags/z
    g config --add versionsort.suffix -rc
    v for-each-ref "$fmt" --sort=version:refname refs/tags/z
    v for-each-ref "$fmt" --sort=-version:refname refs/tags/z
    g config --add versionsort.suffix -beta
    v for-each-ref "$fmt" --sort=v:refname refs/tags/z
    g config --unset-all versionsort.suffix
    g config --add versionsort.suffix -beta
    g config --add versionsort.suffix -pre
    g config --add versionsort.suffix -rc
    v for-each-ref "$fmt" --sort=version:refname refs/tags/z
    # The deprecated key is read when the new one is absent.
    g config --unset-all versionsort.suffix
    g config --add versionsort.prereleasesuffix -rc
    v for-each-ref "$fmt" --sort=version:refname refs/tags/z
    # A plain refname sort ignores the setting.
    v for-each-ref "$fmt" --sort=refname refs/tags/z
}

s_for_each_ref_upstream() {
    fer_repo
    v for-each-ref --format='%(refname:short) up=%(upstream) short=%(upstream:short) track=%(upstream:track) ts=%(upstream:trackshort) nb=%(upstream:track,nobracket) rn=%(upstream:remotename) rr=%(upstream:remoteref)' refs/heads
    v for-each-ref --format='%(refname:short) [%(upstream)] [%(upstream:track)]' refs/remotes refs/tags
    v for-each-ref --format='%(refname:short) push=%(push) pushshort=%(push:short) rn=%(push:remotename)' refs/heads
    g config remote.pushDefault origin; g config branch.loc.pushRemote origin
    v for-each-ref --format='%(refname:short) push=%(push) pushshort=%(push:short)' refs/heads
    v for-each-ref --format='%(upstream:bogus)' refs/heads/main
    v for-each-ref --format='%(HEAD) %(refname:short)' refs/heads
    g switch -q topic
    v for-each-ref --format='%(HEAD) %(refname:short)' refs/heads
}

s_for_each_ref_if() {
    fer_repo
    v for-each-ref --format='%(refname:short): %(if)%(upstream)%(then)tracks %(upstream:short)%(else)no upstream%(end)' refs/heads
    v for-each-ref --format='%(refname:short): %(if:equals=refs/heads/main)%(refname)%(then)MAIN%(else)other%(end)' refs/heads
    v for-each-ref --format='%(refname:short): %(if:notequals=refs/heads/main)%(refname)%(then)not-main%(end)' refs/heads
    v for-each-ref --format='%(refname:short): %(if)%(HEAD)%(then)head%(else)tail%(end)' refs/heads
    v for-each-ref --format='%(refname:short): %(if)%(*objectname)%(then)annotated%(else)plain%(end)' refs/tags
    v for-each-ref --format='%(refname:short): %(if)%(upstream)%(then)%(if:equals=[behind 1])%(upstream:track)%(then)B%(else)T%(end)%(else)N%(end)!' refs/heads
    v for-each-ref --format='%(if)%(authorname)%(then)%(authorname)%(else)(none)%(end) %(refname:short)' refs/tags
    v for-each-ref --format='%(if)%(refname:short)%(then)%(end)x'
    v for-each-ref --format='%(if: )%(upstream:track)%(then)T%(else)E%(end)' refs/heads
    v for-each-ref --format='%(then)x'
    v for-each-ref --format='%(if)x%(then)y'
    v for-each-ref --format='%(if)x%(end)'
    v for-each-ref --format='%(else)x%(end)'
    v for-each-ref --format='%(end)'
    v for-each-ref --format='%(if)x%(then)a%(else)b%(else)c%(end)'
    v for-each-ref --format='%(align:12)%(refname:short)%(end)|%(align:12,right)%(refname:short)%(end)|%(align:12,middle)%(refname:short)%(end)|%(align:4)%(refname:short)%(end)|' refs/heads
    v for-each-ref --format='%(align:width=14,position=right)%(refname:short)%(end)|' refs/heads
    v for-each-ref --format='%(align:6)%(end)|%(align)x%(end)' refs/heads/main
    v for-each-ref --format='%(color:red)%(refname:short)%(color:reset) %(color:bold blue)%(color:green)x%(color:reset)' refs/heads
    v for-each-ref --format='%(refname:short)%x09%(objecttype)%0a%25 %%%(objecttype)' refs/heads/main
    # colours are only emitted when colour is on
    g config color.ui always
    v for-each-ref --color --format='%(color:red)%(refname:short)%(color:reset)|%(color:bold blue)x|%(color:green)y|%(color:ul)z|%(color:red blue)w|%(color:bold)v|%(color:normal)u|%(color:dim italic strike)t|%(color:reverse blink yellow)s%(color:reset)' refs/heads/main
    v for-each-ref --format='%(color:bogus)' refs/heads/main
    v for-each-ref --format='%(color)' refs/heads/main
    v for-each-ref --color=always --format='%(color:magenta)%(refname:short)%(color:reset)' refs/heads/main
    v for-each-ref --color=never --format='%(color:magenta)%(refname:short)%(color:reset)' refs/heads/main
    v for-each-ref --color=auto --format='%(color:magenta)%(refname:short)%(color:reset)' refs/heads/main
    g config color.ui never
    v for-each-ref --format='%(color:red)%(refname:short)%(color:reset)' refs/heads/main
    v for-each-ref --format='%(color:bogus)' refs/heads/main
    v for-each-ref --format='%(refname:short) %(bogus)' refs/heads/main
    v for-each-ref --format='%(if)%(bogus)%(then)a%(end)' refs/heads/main
    v for-each-ref --format='%(if:equals)%(refname)%(then)a%(end)' refs/heads/main
    v for-each-ref --format='%(if:bogus=1)%(refname)%(then)a%(end)' refs/heads/main
    v for-each-ref --format='%(refname' refs/heads/main
    v for-each-ref --format='' refs/heads/main
    v for-each-ref --format='no placeholders' refs/heads/main refs/heads/loc
}

s_for_each_ref_sort() {
    fer_repo
    for key in refname -refname objectname -objectname objecttype -objecttype objectsize -objectsize \
               authordate -authordate committerdate -committerdate taggerdate -taggerdate creatordate -creatordate \
               authorname -authorname committername taggername subject -subject numparent -numparent \
               refname:short -refname:short upstream -upstream upstream:track '*objectname' -'*objectname' tree parent; do
        v for-each-ref --sort="$key" --format="$key %(refname)"
    done
    v for-each-ref --sort=authorname --sort=-refname --format='%(authorname) %(refname)'
    v for-each-ref --sort=-refname --sort=authorname --format='%(authorname) %(refname)'
    v for-each-ref --sort=objecttype --sort=-creatordate --format='%(objecttype) %(refname) %(creatordate:unix)'
    v for-each-ref --sort=bogus
    v for-each-ref --sort=-committerdate --count=3 --format='%(refname)'
    v for-each-ref --sort=refname --count=0
    v for-each-ref --sort=-refname --ignore-case --format='%(refname)' refs/heads
}

s_for_each_ref_filters() {
    fer_repo
    for spec in "--merged main" "--no-merged main" "--merged topic" "--no-merged topic" "--merged=feat" "--no-merged=feat" \
                "--contains main~3" "--no-contains main~3" "--contains topic" "--no-contains topic" "--contains feat" \
                "--contains v1" "--merged v1" "--no-merged v1" "--contains light" "--merged origin/main" \
                "--merged main --no-merged topic" "--merged main --merged feat" "--contains topic --contains feat" \
                "--no-contains topic --no-contains feat" "--contains main~3 --merged main" \
                "--points-at main" "--points-at topic" "--points-at v1" "--points-at v1^{commit}" "--points-at light" \
                "--points-at main~1" "--points-at v1-wrap" "--points-at topic --points-at feat" \
                "--points-at main --contains main~3"; do
        v for-each-ref $spec --format='%(refname)'
    done
    v for-each-ref --format='%(refname)' --merged
    g switch -q feat
    v for-each-ref --format='%(refname)' --merged
    v for-each-ref --format='%(refname)' --no-merged
    v for-each-ref --format='%(refname)' --contains
    v for-each-ref --format='%(refname)' --no-contains
    v for-each-ref --merged --format='%(refname)'
    v for-each-ref --contains --format='%(refname)'
    v for-each-ref --merged refs/heads --format='%(refname)'
    v for-each-ref --merged main refs/heads/f* --format='%(refname)'
    v for-each-ref --merged nosuch --format='%(refname)'
    v for-each-ref --contains nosuch --format='%(refname)'
    v for-each-ref --points-at nosuch --format='%(refname)'
    v for-each-ref --exclude='refs/tags/*' --format='%(refname)'
    v for-each-ref --exclude=refs/heads --format='%(refname)' refs/heads refs/remotes
    v for-each-ref --exclude=refs/remotes/origin/main --exclude='refs/tags/v*' --format='%(refname)'
    v for-each-ref --omit-empty --format='%(upstream)'
    v for-each-ref --format='%(upstream)' refs/heads
    v for-each-ref --count=2 --format='%(refname)'
    v for-each-ref --count=2 --sort=-refname --merged main --format='%(refname)'
}

s_for_each_ref_messages() {
    g init -q repo; cd repo
    local tree; tree=$(g hash-object -t tree -w /dev/null)
    local n=0 text
    for text in "" "
" "only subject" "no trailing newline" "

leading blank lines

then body" "subject

body

" "subj line one
subj line two

para one
para one b

para two
" "	tab subject

	tab body" "a

b

c

d" "trailing spaces   

body   
"; do
        n=$((n + 1))
        tick
        printf '%s' "$text" | g commit-tree "$tree" >"../c$n"
        g update-ref "refs/heads/m$n" "$(cat ../c$n)"
    done
    for n in 3 7; do tick; g tag -a -m "tag $n
second
  indented

last" "t$n" "m$n"; done
    v for-each-ref --format='[%(refname:short)] [%(subject)] [%(body)]' refs/heads refs/tags
    v for-each-ref --format='[%(refname:short)] [%(contents)]' refs/heads refs/tags
    v for-each-ref --format='[%(refname:short)] [%(contents:subject)] [%(contents:body)] [%(contents:signature)]' refs/heads refs/tags
    v for-each-ref --format='[%(refname:short)] [%(contents:lines=1)] [%(contents:lines=3)] [%(contents:lines=0)]' refs/heads refs/tags
    v for-each-ref --sort=subject --format='%(refname:short) [%(subject)]' refs/heads
    v for-each-ref --sort=-body --format='%(refname:short) [%(body)]' refs/heads
    v for-each-ref --sort=contents:subject --format='%(refname:short)' refs/heads
}

# Text with enough alphanumeric characters per line that moved or copied runs clear git's
# thresholds (20 for -M, 40 for -C).
bl_block() { # prefix count
    local i
    for i in $(seq 1 "$2"); do echo "$1 line number $i of the block with words"; done
}
bl_commit() { # name message
    tick; GIT_AUTHOR_NAME="$1" GIT_AUTHOR_EMAIL="$1@x.org" v commit -q -a -m "$2"
}
bl_repo() {
    v init -q repo; cd repo
    { bl_block alpha 6; echo "tiny"; bl_block beta 6; echo "end"; } >a.txt
    { echo "other file"; bl_block gamma 5; } >b.txt
    { echo "untouched"; bl_block delta 7; } >c.txt
    v add .; tick; GIT_AUTHOR_NAME=Root GIT_AUTHOR_EMAIL=root@x.org v commit -q -m root
    # move: the beta block goes to the top of a.txt
    { bl_block beta 6; bl_block alpha 6; echo "tiny"; echo "end"; } >a.txt
    bl_commit Mover "move beta up"
    # copy: a new file takes alpha lines from a.txt while a.txt changes too
    { echo "new header"; bl_block alpha 6; echo "footer"; } >d.txt
    echo "a change" >>a.txt
    v add d.txt; bl_commit Copier "copy alpha to d.txt"
    # copy from an unmodified file (only -C -C looks there)
    { echo "e header"; bl_block delta 7; echo "e footer"; } >e.txt
    v add e.txt; tick; GIT_AUTHOR_NAME=Harder GIT_AUTHOR_EMAIL=harder@x.org v commit -q -m "copy delta from untouched c.txt"
    # a copy in a later commit of an existing file
    { echo "f header"; bl_block gamma 5; } >>b.txt
    bl_commit Later "append gamma copy to b.txt"
}

bl_run() { # every output form for one file/revision
    v blame "$@"
    v blame -M "$@"
    v blame -C "$@"
    v blame -C -C "$@"
    v blame -C -C -C "$@"
    v blame -M50 "$@"
    v blame -C100 "$@"
    v blame -f -C -C -C "$@"
    v blame -s -M -C "$@"
}

s_blame_move_copy() {
    bl_repo
    for f in a.txt b.txt d.txt e.txt; do bl_run "$f"; done
    bl_run HEAD~2 d.txt
    v blame -L 2,5 -C -C a.txt
    v blame -L 1,3 -L 8,10 -M d.txt
}

s_blame_porcelain() {
    bl_repo
    for f in a.txt d.txt b.txt; do
        v blame --porcelain "$f"
        v blame --line-porcelain "$f"
        v blame -p -L 3,9 "$f"
    done
    v blame --porcelain -M a.txt
    v blame --porcelain -C -C d.txt
    v blame --line-porcelain -C -C -C e.txt
    v blame --porcelain HEAD~3 a.txt
    # a rename makes the 'previous' header name the old path
    tick; g mv b.txt bb.txt; v commit -q -m "rename b"
    bl_commit Editor "edit"
    echo extra >>bb.txt; bl_commit Edit2 "edit2"
    v blame --porcelain bb.txt
    v blame --porcelain -f bb.txt
    # merge with a side branch
    v switch -q -c side HEAD~1
    echo "side line" >>c.txt; bl_commit Sider "side"
    v switch -q main
    tick; g merge -q --no-ff -m "merge side" side >/dev/null 2>&1
    v blame --porcelain c.txt
    v blame --porcelain --line-porcelain c.txt
    # uncommitted changes (git stamps them with the current time, which is not compared)
    echo "work tree line" >>c.txt
    DROP_OUT='^(author|committer)-time |^0000000{1,33} \(' v blame --porcelain c.txt
    DROP_OUT='^0{8} \(' v blame c.txt
    DROP_OUT='^0{8} ' v blame -s c.txt
}

# A history with side branches and an octopus merge (one root; see roots_fixture for several).
graph_fixture() {
    v init -q repo; cd repo
    c() { echo "$1" >>"$2"; v add "$2"; tick; v commit -q -m "$1"; }
    c base a
    v branch side1; v branch side2; v branch side3
    c main1 a
    v switch -q side1; c s1a b; c s1b b
    v switch -q side2; c s2a c
    v switch -q main; c main2 a
    tick; v merge -q --no-ff -m "merge side1" side1 >/dev/null
    v switch -q side3; c s3a d; c s3b d
    v switch -q main; c main3 a
    tick; v merge -q --no-ff -m "merge side2" side2 >/dev/null
    tick; v merge -q --no-ff -m "octopus" side3 >/dev/null 2>&1
    c main4 a
    v switch -q main
}

s_log_graph_linear() {
    v init -q repo; cd repo
    for i in 1 2 3 4; do echo "line $i" >f; echo "$i" >>g; v add f g; tick; v commit -q -m "commit $i" -m "body $i"; done
    v log --graph
    v log --graph --oneline
    v log --graph --format='%h %s'
    v log --graph --format='tformat:%h|%s'
    v log --graph --oneline -2
    v log --graph --oneline --skip=1 -2
    v log --graph --stat --oneline
    v log --graph --stat
    v log --graph -p
    v log --graph --name-status --oneline
    v log --graph --reverse
}

s_log_graph_merges() {
    graph_fixture
    v log --graph --oneline
    v log --graph --oneline --all
    v log --graph --all
    v log --graph --format='%h %p %s' --all
    v log --graph --oneline --first-parent
    v log --graph --oneline main..side1
    v log --graph --oneline -3 --all
    # A message ending in its own newline (%n) after a merge or a collapsing row: git adds a row.
    v log --graph --format='%h %s%n' --all
    v log --graph --format='%h %s%n%b' --all
    v log --graph --format='%s%n%n' --all
    v log --graph --format='%h %s%nX%n' --all
    v log --graph --format='tformat:%h%n%b---%n' --all
    v log --graph --format='format:%h %s%n%b' --all
}

s_log_graph_random() {
    v init -q repo; cd repo
    g config gc.auto 0
    RANDOM=7
    echo base >f; v add f; tick; v commit -q -m base
    local n=0 i br k other
    local -a branches=(main)
    for i in $(seq 40); do
        br=${branches[$((RANDOM % ${#branches[@]}))]}
        v switch -q "$br"
        case $((RANDOM % 6)) in
            0|1) n=$((n + 1)); v branch -q "b$n"; branches+=("b$n") ;;
            2|3|4) echo "$i" >>"f$((i % 5))"; v add -A; tick; v commit -q -m "c$i on $br" ;;
            5)
                other=${branches[$((RANDOM % ${#branches[@]}))]}
                if [ "$other" != "$br" ]; then
                    k=${branches[$((RANDOM % ${#branches[@]}))]}
                    tick; g merge -q --no-ff -m "merge $other into $br" "$other" >/dev/null 2>&1 || g merge --abort >/dev/null 2>&1
                    if [ "$k" != "$br" ] && [ "$k" != "$other" ]; then
                        tick; g merge -q --no-ff -m "octopus $other $k into $br" "$other" "$k" >/dev/null 2>&1 || g merge --abort >/dev/null 2>&1
                    fi
                fi ;;
        esac
    done
    v log --graph --oneline --all
    v log --graph --format='%h %p %s' --all
    v log --graph --all
}

# Histories with several root commits: git draws a root that is not shown last shifted to the right.
# Roots are made with real git (`g`); the log under test is `v log`.
roots_fixture() {
    v init -q repo; cd repo
    g config gc.auto 0
    c() { echo "$1" >>"$2"; v add "$2"; tick; v commit -q -m "$1"; }
    root() { g switch -q --orphan "$1"; g rm -rqf . 2>/dev/null; c "$1" "$1"; }
}

s_log_graph_roots() {
    roots_fixture
    # Two, three and four lone roots: the shift climbs one column per root and the last stays put.
    root r1
    root r2
    v log --graph --oneline --all
    root r3
    v log --graph --oneline --all
    root r4
    v log --graph --oneline --all
    v log --graph --all
    v log --graph --format='%h %s%n%b' --all
    v log --graph --oneline --all --topo-order
    v log --graph --oneline --all -3
    v log --graph --oneline --all --skip=1
}

s_log_graph_roots_chains() {
    roots_fixture
    # Roots with a history above them are reached through a column: the connector line leads into
    # the shifted column, and a root followed by a row that continues a column stays where it is.
    root a; c a2 a; c a3 a
    root b; c b2 b
    root c
    g switch -q b; c b3 b
    g switch -q a; c a4 a
    root d; c d2 d; c d3 d
    v log --graph --oneline --all
    v log --graph --all
    v log --graph --format='%h %s%n%b' --all
    v log --graph --stat --oneline --all
    v log --graph --oneline --all -5
}

s_log_graph_roots_merges() {
    roots_fixture
    # Unrelated histories joined by merges: a root reached as a merge's later parent is not shifted,
    # one reached as its first parent is, and lanes beside a shifted root are pushed along with it.
    root a; c a2 a; c a3 a
    root b; c b2 b; c b3 b
    root c; c c2 c
    g switch -q a
    tick; g merge -q --allow-unrelated-histories --no-ff -m "merge b into a" b >/dev/null
    c a4 a
    root d
    g switch -q c
    tick; g merge -q --allow-unrelated-histories --no-ff -m "merge a into c" a >/dev/null
    c c3 c
    g switch -q d; c d2 d
    tick; g merge -q --allow-unrelated-histories --no-ff -m "merge c into d" c >/dev/null
    root e; c e2 e
    v log --graph --oneline --all
    v log --graph --all
    v log --graph --format='%h %p %s' --all
    v log --graph --oneline --all --topo-order
    v log --graph --oneline d
    v log --graph --oneline -6 --all
}

s_log_graph_roots_random() {
    roots_fixture
    c() { echo "$1" >>"$2"; v add "$2"; tick; v commit -q -m "$1"; }
    # A seeded forest: random chains grown on random branches, some joined by merges of unrelated
    # histories, so roots land beside live columns in every arrangement.
    RANDOM=11
    local n=0 i br other
    local -a branches=()
    for i in $(seq 60); do
        case $((RANDOM % 7)) in
            0|1)
                n=$((n + 1)); g switch -q --orphan "t$n"; g rm -rqf . 2>/dev/null
                c "root $n" "f$n"; branches+=("t$n") ;;
            2|3|4)
                [ ${#branches[@]} -gt 0 ] || continue
                br=${branches[$((RANDOM % ${#branches[@]}))]}
                g switch -q "$br"; c "c$i on $br" "f$((i % 4))" ;;
            5|6)
                [ ${#branches[@]} -gt 1 ] || continue
                br=${branches[$((RANDOM % ${#branches[@]}))]}
                other=${branches[$((RANDOM % ${#branches[@]}))]}
                [ "$other" != "$br" ] || continue
                g switch -q "$br"
                tick; g merge -q --allow-unrelated-histories --no-ff -m "merge $other into $br" "$other" >/dev/null 2>&1 || g merge --abort >/dev/null 2>&1 ;;
        esac
    done
    v log --graph --oneline --all
    v log --graph --format='%h %p %s' --all
    v log --graph --all
}

s_log_decorate() {
    graph_fixture
    g config gc.auto 0
    g tag -a annot -m "annotated" main~2
    g tag light side1
    g update-ref refs/remotes/origin/main main~1
    g update-ref refs/stash main~3
    # Git decorates only branches, remote branches, tags and the stash: other namespaces stay hidden.
    g update-ref refs/foo/bar main~2
    g update-ref refs/notes/commits main~2
    v log --oneline --decorate
    v log --decorate
    v log --oneline --decorate=short
    v log --oneline --decorate=full
    v log --oneline --decorate=no
    v log --oneline --no-decorate
    v log --oneline --decorate --all
    v log --decorate --format='%h %s'
    v log --graph --oneline --decorate --all
    v log --graph --decorate --all
    v log --graph --oneline --decorate --stat -3
    v switch -q --detach main~1
    v log --oneline --decorate -4
    v log --oneline --decorate=bogus
}

# An octopus merge whose parents already own columns: git draws the dashes over the columns in use;
# rivet used to index past them and panic (index out of bounds in draw_octopus_merge).
s_log_graph_octopus_shared() {
    v init -q repo; cd repo
    g config gc.auto 0
    local tree
    tree=$(g hash-object -t tree -w /dev/null)
    mk() {
        local name=$1; shift
        local args=() p
        for p in "$@"; do args+=(-p "$(g rev-parse "refs/heads/$p")"); done
        tick
        g update-ref "refs/heads/$name" "$(g commit-tree "$tree" "${args[@]}" -m "$name")"
    }
    mk z
    mk r1c0 z
    mk r0c0 z
    mk r0c1 r0c0
    mk m0 r1c0 r0c1 r0c0
    mk m1 r0c1 r0c0
    v log --graph --oneline --all
    v log --graph --all
    v log --graph --format='%h %p %s' --all
    v log --graph --oneline --topo-order --all
    # more shared columns: two octopus merges stacked over the same branches
    mk b1 r1c0
    mk b2 r0c0
    mk m2 b1 b2 r0c1 m1
    mk m3 m2 r0c1 b1
    v log --graph --oneline --all
    v log --graph --format='%h %p %s' --all
}

s_log_color() {
    graph_fixture
    g config gc.auto 0
    g tag -a annot -m "annotated" main~2
    g tag light side1
    g update-ref refs/remotes/origin/main main~1
    g update-ref refs/stash main~3
    v log --color=always --oneline --decorate
    v log --color=always --decorate
    v log --color=always --oneline
    v log --color=always
    v log --color --oneline --decorate -3
    v log --color=always --oneline --decorate=full -4
    v log --color=always --pretty=oneline --decorate
    v log --color=always --pretty=short --decorate -3
    v log --color=always --pretty=fuller -2
    v log --color=always --format='%h%d %s'
    v log --color=always --oneline --graph --decorate --all
    v log --color=always --graph --decorate --all
    v log --color=always --graph --pretty=fuller --all
    v log --color=always --graph --format='%h %p %s' --all
    v log --color=always --graph --oneline -3
    v log --color=always --graph --oneline main..side1
    v log --color=never --oneline --decorate
    v log --color=auto --graph --oneline --decorate
    v log --color=always --no-color --oneline
    v log --no-color --color=always --oneline -2
    v log --color=bogus --oneline
    v switch -q --detach main~1
    v log --color=always --oneline --decorate -4
    v switch -q main
    # color.ui and the per-slot colours
    g config color.ui always
    v log --oneline --decorate --graph
    v log --oneline --no-color
    v log --oneline --color=never
    g config color.ui true
    v log --oneline --decorate
    g config color.ui false
    v log --oneline --decorate
    v log --oneline --decorate --color=always -2
    g config color.ui always
    g config color.diff.commit "bold blue"
    g config color.decorate.tag red
    g config color.decorate.branch "bold #ff8000"
    g config color.decorate.head cyan
    g config color.decorate.remotebranch 200
    g config color.decorate.stash ul
    v log --oneline --decorate --graph --all
    v log --decorate -3
    g config color.diff.commit normal
    v log --oneline -2
    g config color.diff.commit "ul bold italic"
    v log --oneline -2
    g config color.diff.commit "brightred white"
    v log --oneline -2
    g config color.ui bogus
    v log --oneline -2
}

s_log_graph_roots_color() {
    roots_fixture
    root a; c a2 a
    root b
    root c; c c2 c
    g switch -q a
    tick; g merge -q --allow-unrelated-histories --no-ff -m "merge c into a" c >/dev/null
    root d
    v log --color=always --graph --oneline --all
    v log --color=always --graph --all
    v log --color=always --graph --oneline --decorate --all
}

s_merge_squash() {
    v init -q repo; cd repo
    echo base >a; v add a; tick; v commit -q -m base
    v branch ff
    v switch -q -c side
    echo b >b; v add b; tick; v commit -q -m s1 -m body1
    echo c >>b; tick; v commit -q -a -m s2
    v switch -q main
    echo m >m; v add m; tick; v commit -q -m m1
    # diverged and clean: stage the result, move nothing, leave SQUASH_MSG
    v merge --squash side
    v status --short
    v log --oneline
    v commit -q -m "squashed side"
    v log --format='%h %p %s'
    v status --short
    # nothing new to squash
    v merge --squash side
    v status --short
    # the branch is behind the other: fast-forward-able, still not committed
    v switch -q ff
    v merge --squash side
    v status --short
    v log --oneline
    v reset -q --hard
    # option clashes and usage
    v merge --squash --no-ff side
    v merge --squash --ff-only side
    v merge --squash --no-commit side
    v status --short
    v reset -q --hard
    v merge --squash -q side
    v status --short
    v reset -q --hard
    v merge --squash nosuchbranch
    # a conflict: the index keeps the stages, no MERGE_HEAD, and a plain commit finishes it
    v switch -q main
    v switch -q -c clash
    echo one >c1; v add c1; tick; v commit -q -m c1base
    v switch -q -c clash-side
    echo two >c1; tick; v commit -q -a -m c1side
    v switch -q clash
    echo three >c1; tick; v commit -q -a -m c1here
    v merge --squash clash-side
    v status --short
    v ls-files -s
    v commit -q -m "unresolved"
    echo resolved >c1; v add c1
    v commit -q -m "resolved squash"
    v log --format='%h %p %s' -3
    v status --short
    # up to date, quietly; and SQUASH_MSG does not survive a branch switch
    v merge --squash -q main
    v switch -q ff
    v merge --squash -q side
    v switch -q -c elsewhere
    v status --short
    v commit -q -m "after the switch"
    v log --format='%h %p %s' -2
}

s_pull_rebase() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    v clone -q origin.git repo
    cd repo
    v pull --rebase
    # upstream moved, nothing local to replay
    (cd ../seed && tick && echo b >b && g add b && g commit -qm two && g push -q origin HEAD:main 2>/dev/null)
    v pull --rebase
    v log --format='%h %p %s'
    # upstream moved and two local commits are replayed on top
    (cd ../seed && tick && echo c >c && g add c && g commit -qm three && g push -q origin HEAD:main 2>/dev/null)
    tick; echo l1 >l1; v add l1; v commit -q -m local1
    tick; echo l2 >l2; v add l2; v commit -q -m local2
    v pull --rebase
    v log --format='%h %p %s'
    v status --short
    v reflog
    # a dirty tree is refused, with git's wording
    echo dirty >>a
    (cd ../seed && tick && echo d >d && g add d && g commit -qm four && g push -q origin HEAD:main 2>/dev/null)
    v pull --rebase
    v checkout -q -- a
    # nothing upstream, local commits only
    v pull --rebase
    v log --format='%h %p %s' -3
    # without a policy a divergent pull is refused; --ff-only refuses too
    tick; echo l3 >l3; v add l3; v commit -q -m local3
    (cd ../seed && tick && echo e >e && g add e && g commit -qm five && g push -q origin HEAD:main 2>/dev/null)
    v pull
    v pull --no-rebase --ff-only
    g config pull.rebase true
    v pull
    v log --format='%h %p %s' -3
    v status --short
}

s_pull_rebase_conflict() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo base >f && g add f && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    v clone -q origin.git repo
    cd repo
    (cd ../seed && tick && echo upstream >f && g commit -qam up && g push -q origin HEAD:main 2>/dev/null)
    tick; echo mine >f; v commit -q -a -m mine
    tick; echo more >g; v add g; v commit -q -m more
    v pull --rebase
    v status --short
    echo resolved >f; v add f
    GIT_EDITOR=true v rebase --continue
    v log --format='%h %p %s'
    v status --short
}

s_pull_squash() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    v clone -q origin.git repo
    (cd seed && tick && echo b >b && g add b && g commit -qm two && g push -q origin HEAD:main 2>/dev/null)
    cd repo
    v pull --squash
    v status --short
    v log --oneline
    v commit -q -m "squashed pull"
    (cd ../seed && tick && echo c >c && g add c && g commit -qm three && g push -q origin HEAD:main 2>/dev/null)
    tick; echo l >l; v add l; v commit -q -m local
    v pull --squash
    v status --short
    v log --oneline
}

# Unmerged paths without MERGE_HEAD: git prints "<path>: needs merge" on stdout and only the
# "error: you need to resolve ..." line on stderr, for both checkout and switch.
s_switch_unmerged_stream() {
    v init -q repo; cd repo
    echo base >c1; v add c1; v commit -q -m base
    v branch other; v branch side
    echo main >c1; v commit -q -a -m main
    v switch -q side; echo side >c1; v commit -q -a -m side
    v switch -q main
    v merge side
    rm -f .git/MERGE_HEAD
    v status --short
    v switch other
    v checkout other
    v switch --detach other
    v status --short
}

# `checkout --orphan` creates no reflog entry: neither HEAD's log nor the new branch's own log
# gets a line (compare .git/logs/HEAD and the absence of logs/refs/heads/<name> via the state).
s_checkout_orphan_reflog() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    tick; echo b >b; v add b; v commit -q -m two
    v checkout --orphan fresh
    v status --short
    cat .git/HEAD
    cat .git/logs/HEAD
    test -e .git/logs/refs/heads/fresh && echo "branch log exists" || echo "no branch log"
    tick; v commit -q -m root
    v reflog show HEAD
    v reflog show fresh
    v log --oneline
    v switch --orphan second
    v status --short
    v checkout main
    v reflog show HEAD
}

# `pull --rebase` replays only the commits after the fork point: when upstream rewrote a commit
# that is already in the local branch, that old commit is dropped (git's `merge-base --fork-point`
# over the upstream branch's reflog), not replayed on top of its own rewrite.
s_pull_rebase_fork_point() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && g add a && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    v clone -q origin.git repo
    cd repo
    (cd ../seed && tick && echo b >b && g add b && g commit -qm two && g push -q origin HEAD:main 2>/dev/null)
    v fetch -q
    v merge -q --ff-only origin/main
    tick; echo l1 >l1; v add l1; v commit -q -m local1
    tick; echo l2 >l2; v add l2; v commit -q -m local2
    # upstream rewrites "two" and adds a commit on top of the rewrite
    (cd ../seed && g reset -q --hard HEAD~1 && tick && echo b2 >b2 && g add b2 && g commit -qm two-rewritten && tick && echo c >c && g add c && g commit -qm three && g push -q -f origin HEAD:main 2>/dev/null)
    v pull --rebase
    v log --format='%h %p %s'
    v status --short
    v reflog
    v ls-files
    # rewritten once more, no local commits left to keep except the replayed ones
    (cd ../seed && g reset -q --hard HEAD~1 && tick && echo c2 >c2 && g add c2 && g commit -qm three-rewritten && g push -q -f origin HEAD:main 2>/dev/null)
    v pull --rebase
    v log --format='%h %p %s'
    v status --short
    v reflog
}

# merge-base --fork-point: the best common ancestor of a commit and every tip the ref's reflog
# recorded, only if there is exactly one and it is one of those recorded tips.
s_merge_base_fork_point() {
    g init -q repo; cd repo
    echo 0 >f; g add f; g commit -q -m root
    tick; echo 1 >f; g commit -q -a -m a1; a1=$(g rev-parse HEAD)
    tick; echo 2 >f; g commit -q -a -m a2; a2=$(g rev-parse HEAD)
    g branch up "$a1"
    g update-ref -m step1 refs/heads/up "$a2"
    g checkout -q -b mine "$a1"
    tick; echo m >m; g add m; g commit -q -m mine1
    tick; echo m2 >>m; g commit -q -a -m mine2
    # upstream rewritten: a1 -> a2 -> b1 (unrelated to a2)
    g checkout -q "$(g rev-parse HEAD~2)"
    tick; echo rewritten >f; g commit -q -a -m b1; b1=$(g rev-parse HEAD)
    g update-ref -m rewrite refs/heads/up "$b1"
    g checkout -q mine
    v merge-base --fork-point up
    v merge-base --fork-point up mine
    v merge-base --fork-point refs/heads/up HEAD~1
    v merge-base --fork-point up "$a2"
    v merge-base --fork-point up root 2>/dev/null
    v merge-base --fork-point nosuchref
    v merge-base --fork-point up nosuchcommit
    v merge-base --fork-point
    v merge-base --fork-point --all up
    # a ref without a reflog uses its current value
    g tag lone "$a1"
    v merge-base --fork-point lone
    v merge-base --fork-point lone "$b1"
    # more than one best common ancestor: no fork point
    g checkout -q -b left "$a1"; tick; echo l >l; g add l; g commit -q -m left
    g checkout -q -b right "$a1"; tick; echo r >r; g add r; g commit -q -m right
    g checkout -q -b crossa left; g merge -q --no-edit right >/dev/null 2>&1
    g checkout -q -b crossb right; g merge -q --no-edit left >/dev/null 2>&1
    g update-ref -m x refs/heads/watch crossa; g update-ref -m y refs/heads/watch crossb
    v merge-base --fork-point watch left
    v merge-base --fork-point watch HEAD
}

# --autostash: tracked local changes are stashed before the operation and put back after it;
# when putting them back conflicts, the stash entry stays (stash list, status and markers match).
s_pull_autostash() {
    g init -q --bare origin.git
    g clone -q origin.git seed 2>/dev/null
    (cd seed && echo a >a && echo f >f && g add a f && g commit -qm one && g push -q origin HEAD:main 2>/dev/null)
    v init -q repo
    (cd repo && v remote add origin ../origin.git && v fetch -q origin && v switch -q main)
    cd repo
    # fast-forward under --rebase, dirty tracked file
    (cd ../seed && tick && echo b >b && g add b && g commit -qm two && g push -q origin HEAD:main 2>/dev/null)
    echo dirty >>a
    v pull --rebase --autostash
    v status --short
    v stash list
    v reflog
    # replayed local commit, staged and unstaged changes
    tick; echo l1 >l1; v add l1; v commit -q -m local1
    (cd ../seed && tick && echo c >c && g add c && g commit -qm three && g push -q origin HEAD:main 2>/dev/null)
    echo staged >>f; v add f
    v pull --rebase --autostash
    v status --short
    cat a f
    v stash list
    v log --format='%h %p %s' -3
    # applying conflicts: the stash entry stays
    v checkout -q -- a f
    (cd ../seed && tick && echo up >a && g commit -qam four && g push -q origin HEAD:main 2>/dev/null)
    tick; echo l2 >l2; v add l2; v commit -q -m local2
    echo mine >a
    v pull --rebase --autostash
    v status --short
    cat a
    v stash list
    v stash show -p
    v checkout -q -f HEAD
    v stash drop -q
    # a plain (merge) pull
    (cd ../seed && tick && echo d >d && g add d && g commit -qm five && g push -q origin HEAD:main 2>/dev/null)
    echo dirty2 >>a
    v pull --autostash --no-rebase
    v status --short
    v log --format='%h %p %s' -2
    v stash list
    tick; echo l3 >l3; v add l3; v commit -q -m local3
    (cd ../seed && tick && echo e >e && g add e && g commit -qm six && g push -q origin HEAD:main 2>/dev/null)
    v pull --autostash --no-rebase
    v status --short
    v log --format='%h %p %s' -3
    v stash list
    # nothing to stash: no message
    v checkout -q -- a
    (cd ../seed && tick && echo g >g && g add g && g commit -qm seven && g push -q origin HEAD:main 2>/dev/null)
    v pull --rebase --autostash
    v stash list
    # without the flag a dirty tree is still refused
    echo dirty3 >>a
    v pull --rebase
    g config rebase.autoStash true
    (cd ../seed && tick && echo h >h && g add h && g commit -qm eight && g push -q origin HEAD:main 2>/dev/null)
    v pull --rebase
    v status --short
}

s_rebase_autostash() {
    v init -q repo; cd repo
    echo a >a; echo f >f; v add a f; v commit -q -m one
    v branch up
    tick; echo x >x; v add x; v commit -q -m two
    echo dirty >>a
    # up to date: stashed and put back
    v rebase --autostash main
    v status --short
    v stash list
    # a real rebase with the stash applied at the end
    v switch -q up; tick; echo y >y; v add y; v commit -q -m upc; v switch -q main
    echo dirty2 >>a
    v rebase --autostash up
    v status --short
    cat a
    v stash list
    v reflog -4
    # a conflict during the replay: the stash waits until --continue
    v reset -q --hard HEAD
    v switch -q up; tick; echo conf >f; v commit -q -a -m upf; v switch -q main
    tick; echo mine >f; v commit -q -a -m minef
    echo dirty3 >>a
    v rebase --autostash up
    v status --short
    v stash list
    echo res >f; v add f
    GIT_EDITOR=true v rebase --continue
    v status --short
    cat a
    v stash list
    # and --abort puts it back too
    v reset -q --hard HEAD
    v switch -q up; tick; echo c2 >f; v commit -q -a -m up2; v switch -q main
    tick; echo m2 >f; v commit -q -a -m m2
    echo dirty4 >>a
    v rebase --autostash up
    v rebase --abort
    v status --short
    cat a
    v stash list
    # the config switch, and --no-autostash over it
    g config rebase.autoStash true
    echo dirty5 >>a
    v rebase up
    v status --short
    v rebase --abort
    v reset -q --hard HEAD
    echo dirty6 >>a
    v rebase --no-autostash up
    v status --short
}

s_merge_autostash() {
    v init -q repo; cd repo
    echo a >a; echo f >f; v add a f; v commit -q -m one
    v branch side; v branch clean
    tick; echo mainf >f; v commit -q -a -m mainf
    v switch -q side; tick; echo sidef >f; v commit -q -a -m sidef
    v switch -q clean; tick; echo z >z; v add z; v commit -q -m zz
    v switch -q main
    # clean merge
    echo dirty >>a
    v merge --autostash clean
    v status --short
    v stash list
    v log --format='%h %p %s' -2
    # up to date
    v merge --autostash clean
    v status --short
    # conflicting merge: the stash waits in MERGE_AUTOSTASH until the merge is concluded
    v merge --autostash side
    v status --short
    v stash list
    echo res >f; v add f
    v commit -q -m merged
    v status --short
    cat a
    v stash list
    # abort
    v reset -q --hard HEAD~1
    v switch -q side; v branch -f other; v switch -q main
    echo dirty2 >>a
    v merge --autostash side
    v merge --abort
    v status --short
    cat a
    v stash list
    # quit and reset keep the stash
    echo dirty3 >>a
    v merge --autostash side
    v merge --quit
    v status --short
    v stash list
    v reset -q --hard HEAD
    v stash drop -q
    echo dirty4 >>a
    v merge --autostash side
    v reset -q --hard HEAD
    v status --short
    v stash list
    # a fast-forward
    v stash drop -q
    v switch -q -c ff clean
    tick; echo w >w; v add w; v commit -q -m ww
    v switch -q clean
    echo dirty5 >>a
    v merge --autostash ff
    v status --short
    v stash list
}

# Naming HEAD (or @) keeps the branch checked out: no "Already on", only the local changes.
s_checkout_head() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v checkout HEAD
    cat .git/HEAD
    echo dirty >>a
    v checkout HEAD
    v checkout @
    v checkout -q HEAD
    v checkout -f HEAD
    cat a
    v checkout --detach HEAD
    cat .git/HEAD
    v checkout HEAD
    cat .git/HEAD
    v reflog
}

# Conflict kinds other than plain content during a rebase, and the steps around them.
s_rebase_conflict_kinds() {
    v init -q repo; cd repo
    printf '1\n2\n3\n' >f; printf 'x\n' >gone; printf 'r1\nr2\nr3\nr4\nr5\nr6\n' >old; echo k >keep; v add .; v commit -q -m base
    v branch topic
    tick; git rm -q gone; echo added >new; v add new; git mv old moved; printf 'r1\nr2\nr3\nr4\nr5\nmain\n' >moved; git add moved; v commit -q -m "main: delete, add, rename"
    v switch -q topic
    tick; echo changed >gone; v commit -q -a -m "topic edits gone"
    tick; echo other >new; v add new; v commit -q -m "topic adds new"
    tick; printf 'topic\nr2\nr3\nr4\nr5\nr6\n' >old; v commit -q -a -m "topic edits old"
    v rebase main
    v status
    v status --short
    git rm -q -f gone 2>/dev/null
    v rebase --continue
    v status --short
    cat new
    echo merged >new; git add new
    v rebase --continue
    v status --short
    v diff --stat HEAD
    ls
    v rebase --quit
    v rebase --abort
    v status
    v rebase --show-current-patch
    v log --oneline --graph --all
    # stop again: look at the patch, then walk away with --quit
    v reset -q --hard "$(g rev-parse topic@{1})" 2>/dev/null || v reset -q --hard HEAD
    v switch -q topic
    v rebase main
    v rebase --show-current-patch
    v rebase --quit
    ls .git | grep -i -E 'rebase|AUTO_MERGE|MERGE_MSG'
    v status --short
    v reflog -3
    v rebase --abort
}

# pack-refs: tags by default, everything with --all, loose files pruned unless --no-prune.
s_pack_refs() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v tag light; v tag -a ann -m annotated; v tag -a nested -m nested ann
    v branch b1; v branch b2; v branch feature/x
    g update-ref refs/notes/x HEAD; g update-ref refs/remotes/o/main HEAD
    refs_on_disk() { { echo "## loose"; find .git/refs -type f | sort; echo "## packed"; cat .git/packed-refs 2>/dev/null; } >>"$TRANSCRIPT"; }
    v pack-refs
    refs_on_disk
    v pack-refs --all
    refs_on_disk
    tick; echo b >b; v add b; v commit -q -m two
    v branch b3
    v tag newtag
    v pack-refs --no-prune
    refs_on_disk
    v pack-refs
    refs_on_disk
    v tag -d newtag
    v branch -D b1
    refs_on_disk
    v for-each-ref
    v pack-refs --all --exclude 'refs/heads/b*'
    refs_on_disk
    v pack-refs --include 'refs/heads/b*'
    refs_on_disk
    v log --oneline --all
    v show-ref
    v pack-refs --all --no-prune
    v pack-refs --all
    refs_on_disk
    v fsck
    v gc
    refs_on_disk
}

# repack: loose objects into a pack, everything with -a/-A, -d dropping what is replaced.
s_repack() {
    g config --global gc.cruftPacks false
    g config --global pack.threads 1
    v init -q repo; cd repo
    seq 1 200 | sed 's/^/line /' >big.txt; cp big.txt copy.txt
    mkdir src; for i in 1 2 3; do seq 1 80 | sed "s/^/mod$i /" >src/m$i.c; done
    v add .; v commit -q -m one
    for n in 2 3 4 5; do
        tick; sed -i "$((n * 10))s/.*/changed at rev $n/" big.txt; echo "extra $n" >>copy.txt
        v commit -q -a -m "rev $n"
    done
    v tag -a v1 -m annotated
    g gc -q
    packshape
    # new loose history; an unreachable object; a blob only the index knows
    for n in 6 7; do
        tick; sed -i "$((n * 10))s/.*/changed at rev $n/" big.txt; echo "extra $n" >>copy.txt
        v commit -q -a -m "rev $n"
    done
    v tag light
    echo unreachable-blob | g hash-object -w --stdin >/dev/null
    echo index-only >idx.txt; v add idx.txt
    allobjects
    v repack
    packshape; allobjects
    v repack
    v repack -q
    v repack -d
    packshape; allobjects
    # everything into one pack, the old ones kept
    v repack -a
    packshape; allobjects
    v repack -a -d
    packshape; allobjects
    v fsck
    v count-objects -v
    # unreachable objects of a replaced pack: dropped, loosened (-A) or kept (-k)
    tick; echo x >x; v add x; v commit -q -m x
    v repack -d
    v reset -q --hard HEAD~1
    g reflog expire --expire=now --all
    v repack -a -d
    packshape; allobjects
    tick; echo y >y; v add y; v commit -q -m y
    v repack -d
    v reset -q --hard HEAD~1
    g reflog expire --expire=now --all
    v repack -A -d
    packshape; allobjects
    tick; echo z >z; v add z; v commit -q -m z
    v repack -d
    v reset -q --hard HEAD~1
    g reflog expire --expire=now --all
    v repack -a -d -k
    packshape; allobjects
    v repack -a -d -f
    packshape
    v fsck
}

# repack -A with unreachable objects spread over several packs and loose objects; .keep packs.
s_repack_keep_multi() {
    g config --global gc.cruftPacks false
    g config --global pack.threads 1
    v init -q repo; cd repo
    seq 1 100 | sed 's/^/line /' >f; v add f; v commit -q -m base
    v branch -q keepme
    for round in 1 2 3; do
        v switch -q -c r$round
        for n in 1 2 3; do tick; echo "round $round n $n" >>f; echo "r${round}n$n" >"file$round$n"; v add .; v commit -q -m "r$round-$n"; done
        v repack -q -d
        v switch -q main; v branch -q -D r$round
    done
    for i in 1 2; do echo "loose $i" | g hash-object -w --stdin >/dev/null; done
    g reflog expire --expire=now --all
    packshape; allobjects
    v repack -A -d
    packshape; allobjects
    v fsck
    # a .keep pack stays and its objects are not repacked
    seq 1 50 >g1; v add g1; v commit -q -m g1
    v repack -q -d
    for k in .git/objects/pack/*.pack; do : >"${k%.pack}.keep"; done
    tick; echo g2 >g2; v add g2; v commit -q -m g2
    v repack -a -d
    packshape; allobjects
    v repack -A
    packshape; allobjects
}

# The bisect state git keeps: refs and files.
bisect_snap() {
    {
        echo "## bisect state"
        g for-each-ref refs/bisect
        for f in BISECT_START BISECT_LOG BISECT_TERMS BISECT_NAMES BISECT_EXPECTED_REV BISECT_ANCESTORS_OK; do
            if [ -e ".git/$f" ]; then echo "# $f"; cat ".git/$f"; fi
        done
        echo "# HEAD"; cat .git/HEAD
    } >>"$TRANSCRIPT"
}

bisect_repo() {
    v init -q repo; cd repo
    for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
        tick; echo $i >f
        if [ $i -ge 8 ]; then echo bug >bug; fi
        if [ $i = 5 ]; then echo extra >extra; fi
        v add -A; v commit -q -m "c$i"
    done
}

s_bisect_basic() {
    bisect_repo
    v bisect good
    v bisect reset
    v bisect log
    v bisect terms
    v bisect start
    bisect_snap
    v bisect bad HEAD
    v bisect log
    v bisect good main~11
    bisect_snap
    v bisect good
    bisect_snap
    v bisect bad
    v bisect good
    bisect_snap
    v bisect bad
    v bisect good
    bisect_snap
    v bisect log
    v bisect reset
    bisect_snap
    v status -sb
    v bisect reset
    v bisect
    v bisect frobnicate
}

s_bisect_start_args() {
    bisect_repo
    v bisect start main main~11 main~10
    bisect_snap
    v bisect skip
    v bisect skip
    bisect_snap
    v bisect good
    v bisect bad
    v bisect log
    v bisect reset main~3
    bisect_snap
    v status -sb
    v switch -q main
    v bisect start main~1 main~9 --
    v bisect bad main~4
    v bisect skip main~7..main~5
    bisect_snap
    v bisect next
    v bisect reset
    v bisect start --bogus
    v bisect bad 1 2
}

s_bisect_terms() {
    bisect_repo
    v bisect start --term-old=fast --term-new=slow
    v bisect terms
    v bisect slow main
    v bisect fast main~11
    v bisect terms --term-new
    v bisect terms --term-old
    v bisect bad
    v bisect new
    bisect_snap
    v bisect fast
    v bisect log
    v bisect reset
    v bisect start
    v bisect new HEAD
    v bisect old main~11
    v bisect terms
    v bisect good
    v bisect old
    bisect_snap
    v bisect new
    v bisect reset
}

s_bisect_run() {
    bisect_repo
    printf '#!/bin/sh\n[ "$(cat f)" -lt 8 ]\n' >../check.sh; chmod +x ../check.sh
    v bisect start main main~11
    v bisect run ../check.sh
    bisect_snap
    v bisect log
    v bisect log >../saved.log
    cp .git/BISECT_LOG ../saved.log
    v bisect reset
    v bisect replay ../saved.log
    bisect_snap
    v bisect reset
    printf '#!/bin/sh\ntest -e bug && exit 125; [ "$(cat f)" -lt 6 ]\n' >../skip.sh; chmod +x ../skip.sh
    v bisect start main main~11
    v bisect run ../skip.sh
    v bisect reset
    v bisect run true
    v bisect start main main~11
    v bisect run
    v bisect run sh -c 'exit 130'
    v bisect reset
}

s_bisect_visualize() {
    bisect_repo
    unset DISPLAY SESSIONNAME MSYSTEM SECURITYSESSIONID
    v bisect visualize
    v bisect start
    v bisect visualize
    v bisect bad main
    v bisect view --oneline
    v bisect good main~11
    v bisect visualize --oneline
    v bisect view --oneline --reverse
    v bisect view -n2 --format=%s
    v bisect view --oneline -- f
    bisect_snap
    v bisect good
    v bisect view --oneline
    v bisect skip
    v bisect view --oneline
    v bisect reset
    v bisect view --oneline
    v bisect start --term-old=fast --term-new=slow main main~5
    v bisect view --oneline
    v bisect reset
}

s_cherry() {
    v init -q repo; cd repo
    tick; printf 'a\nb\nc\nd\ne\nf\n' >f; v add f; v commit -q -m base
    v branch -q topic
    tick; echo main1 >m1; v add m1; v commit -q -m "main only"
    v switch -q topic
    tick; echo one >t1; v add t1; v commit -q -m "topic one"
    tick; sed -i 's/^c$/C/' f; v add f; v commit -q -m "topic two: upcase c"
    tick; echo three >t3; v add t3; v commit -q -m "topic three"
    v switch -q main
    v cherry-pick topic~1
    tick; echo main2 >m2; v add m2; v commit -q -m "main two"
    v cherry-pick topic~2
    v switch -q topic
    v cherry main
    v cherry -v main
    v cherry -v main topic
    v cherry main topic~1
    v cherry main topic topic~2
    v cherry topic main
    v cherry -v topic main
    v cherry
    v cherry main topic topic~2 extra
    v cherry nosuch
    v cherry main nosuch
    v branch -q --set-upstream-to=main
    v cherry
    v cherry -v
    tick; v merge -q --no-ff -m "merge main" main
    v cherry -v main
    v cherry -v HEAD~1 HEAD
}

# Linked work trees made by real git, then used by the implementation under test.
s_worktree_linked() {
    v init -q repo; cd repo
    tick; printf 'a\nb\n' >f; v add f; v commit -q -m one
    tick; echo two >>f; v add f; v commit -q -m two
    v branch -q side
    g worktree add -q ../wt side
    g worktree add -q --detach ../wd
    cd ../wt
    v status -sb
    v branch
    v log --oneline
    echo wt >w.txt; v add w.txt; tick; v commit -q -m "in worktree"
    v log --oneline -2
    v status -sb
    v switch -q -c wtbranch
    v branch
    v rev-parse --git-dir HEAD
    v rev-parse --show-toplevel HEAD
    v config worktree.test yes
    v stash list
    echo more >>f; v stash -q
    v stash list
    v stash pop -q
    v checkout -q --detach HEAD~1
    v reflog -3
    cd ../repo
    v branch -a
    v log --oneline --all
    v status -sb
    v config worktree.test
    cd ../wd
    v status -sb
    v merge -q side
    v log --oneline -3
    cd ../wt
    v log --oneline -3
    v fsck
}

# What `worktree` leaves in the administrative directories.
wt_snap() {
    {
        echo "## worktree admin"
        if [ -d .git/worktrees ]; then
            (cd .git/worktrees && find . -mindepth 1 ! -name index | sort | while read -r f; do
                if [ -d "$f" ]; then echo "d $f"; else echo "# $f"; sed "s#$ROOT#<ROOT>#g" "$f"; fi
            done)
        else
            echo "(no worktrees dir)"
        fi
    } >>"$TRANSCRIPT"
}

s_worktree_basic() {
    v init -q repo; cd repo
    tick; printf 'a\nb\n' >f; v add f; v commit -q -m one
    tick; echo two >>f; v add f; v commit -q -m two
    v worktree
    v worktree frobnicate
    v worktree list
    v worktree list --porcelain
    v worktree add ../w1
    v worktree add -q ../w9
    v worktree add -b nb ../w2 HEAD~1
    v worktree add --detach ../w3
    v worktree add ../w1
    v worktree add ../w4 main
    v worktree add -b w1 ../other
    v worktree add ../m/w1 -b newone
    v worktree add --lock --reason busy ../lk
    v worktree add ../zz nosuch
    v worktree add
    v worktree add --no-checkout ../nc
    wt_snap
    v worktree list
    v worktree list -v
    v worktree list --porcelain
    v worktree list --porcelain -z
    v branch
    (cd ../w1 && v status -sb)
    (cd ../w2 && v log --oneline)
}

s_worktree_lock_remove() {
    v init -q repo; cd repo
    tick; printf 'a\nb\n' >f; v add f; v commit -q -m one
    v worktree add -q ../c
    v worktree add -q ../d
    v worktree add -q ../e
    v worktree add -q ../g
    echo x >../c/untracked
    v worktree remove ../c
    echo y >>../d/f
    v worktree remove ../d
    v worktree remove -f ../c
    v worktree remove -f ../d
    wt_snap
    v worktree remove ../nosuch
    v worktree remove .
    v worktree remove
    v worktree lock ../g --reason "why not"
    v worktree lock ../g
    v worktree list
    v worktree list -v
    v worktree list --porcelain
    v worktree remove ../g
    v worktree remove -f ../g
    v worktree remove -f -f ../g
    v worktree unlock ../e
    v worktree unlock ../e
    v worktree lock ../e
    v worktree lock ../nosuch
    v worktree lock .
    v worktree unlock .
    v worktree unlock
    v worktree lock
    v worktree remove ../e
    wt_snap
    v branch
    v worktree list
}

s_worktree_prune() {
    v init -q repo; cd repo
    tick; printf 'a\nb\n' >f; v add f; v commit -q -m one
    v worktree add -q ../a
    v worktree add -q ../b
    v worktree add -q ../c
    v worktree add -q ../d
    v worktree lock ../c --reason keep
    rm -rf ../a ../c
    v worktree list
    v worktree list -v
    v worktree list --porcelain
    v worktree prune -n
    v worktree prune -n -v
    wt_snap
    v worktree prune -v
    wt_snap
    rm -rf ../b ../d
    v worktree prune
    v worktree unlock ../c
    v worktree prune
    wt_snap
    v worktree list
    v worktree prune extra
    v worktree prune -v
}

s_worktree_branch_guards() {
    v init -q repo; cd repo
    tick; printf 'a\nb\n' >f; v add f; v commit -q -m one
    tick; echo two >>f; v add f; v commit -q -m two
    v branch -q spare
    v worktree add -q -b other ../o
    v worktree add -q --detach ../d
    v branch
    v branch -v
    v branch -D other
    v branch -d other
    v branch -m other zzz
    v worktree list
    v switch zzz
    v checkout zzz
    v switch -q spare
    (cd ../o && v switch main)
    (cd ../o && v checkout main)
    (cd ../o && v switch -q spare)
    (cd ../d && v branch -d main)
    (cd ../d && v branch)
    v branch -f zzz HEAD~1
    v branch -f spare HEAD~1
    v branch -m spare spare2
    v branch -m zzz yyy
    v worktree list
    v branch -D yyy
    v switch -q main
    (cd ../o && tick && echo o >o.txt && v add o.txt && v commit -q -m "on o")
    v branch -v
    v log --oneline --all
    v switch --detach yyy
    v worktree list
}

s_worktree_force_guards() {
    v init -q repo; cd repo
    tick; echo a >f; v add f; v commit -q -m a
    tick; echo b >f; v add f; v commit -q -m b
    v worktree add -q -b other ../o
    v branch -f other HEAD~1
    v branch -f main HEAD~1
    v switch -C other
    v checkout -B other HEAD


    v branch -m other main2
    v branch -m main other2
    v branch --force -m other2 main2
    v branch -M other2 other
    v branch -m other2 spare
    v worktree list
    v branch
}

# `gc` in the main work tree must keep what only another work tree reaches: a detached HEAD, a
# staged blob, and the commits behind its HEAD reflog.
s_worktree_gc_keeps() {
    v init -q repo; cd repo
    tick; printf 'a\nb\n' >f; v add f; v commit -q -m one
    v worktree add -q --detach ../d
    (cd ../d && tick && echo x >x.txt && v add x.txt && v commit -q -m "only on the detached head")
    (cd ../d && echo staged-only >s.txt && v add s.txt)
    (cd ../d && v checkout -q --detach HEAD~1 && v checkout -q --detach main)
    v worktree add -q --detach ../e
    (cd ../e && tick && echo y >y.txt && v add y.txt && v commit -q -m "only on e")
    (cd ../e && v checkout -q --detach main)
    v gc --prune=now
    (cd ../d && v fsck && v status -sb && v log --oneline)
    (cd ../d && v diff --cached)
    (cd ../e && v reflog)
    v count-objects
    v repack -a -d -q
    (cd ../d && v fsck && v diff --cached)
    v worktree prune
}

# What an archive file holds, as the tools that read it see it. The raw bytes are compared for
# tar and for stored zip entries; for compressed data only what it decompresses to is.
arch_snap() {
    local file=$1
    {
        echo "## archive $(basename "$file")"
        case $file in
            *.tar)
                (cd "$(dirname "$file")" && sha1sum "$(basename "$file")")
                tar tvf "$file" ;;
            *.tgz|*.tar.gz)
                head -c10 "$file" | xxd -p
                gzip -dc "$file" | sha1sum
                gzip -dc "$file" | tar tvf - ;;
            *.zip)
                unzip -z "$file"
                zipinfo "$file" | grep -E '^[-dl][-rwx]' | awk '{print $1,$2,$3,$4,$5,$7,$8,$9,$10}'
                zipinfo -1 "$file" | while read -r n; do printf '%s ' "$n"; unzip -p "$file" "$n" | sha1sum; done ;;
        esac
    } >>"$TRANSCRIPT" 2>&1
}

arch_repo() {
    v init -q repo; cd repo
    local long="$(printf 'd%.0s' $(seq 1 60))/$(printf 'e%.0s' $(seq 1 60))/$(printf 'f%.0s' $(seq 1 30))"
    mkdir -p "$long" docs tests build vendor/x src/deep
    tick; echo hello >README; printf '#!/bin/sh\n' >run.sh; chmod +x run.sh
    echo code >src/a.c; echo deep >src/deep/b.c; : >empty
    echo deep >"$long/file.txt"; echo one >"$long/$(printf 'g%.0s' $(seq 1 120)).txt"
    echo caf >"café.txt"
    head -c 20000 /dev/zero | tr '\0' 'z' >docs/big.txt; seq 1 3000 >docs/nums.txt; chmod +x docs/nums.txt
    echo t >tests/t.c; echo b >build/out.o; echo v >vendor/x/lib.c; echo keep >vendor/keep.txt
    printf '/build export-ignore\ntests export-ignore\nvendor/x export-ignore\n*.o export-ignore\n' >.gitattributes
    ln -s README link; ln -s "$(printf 'L%.0s' $(seq 1 150))" longlink
    v add -A; v commit -q -m c1
    tick; echo more >>README; v add README; v commit -q -m c2
    g update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,sub
    tick; v commit -q -m c3
    v tag -a -m "tag v1" v1
    v tag light
}

s_archive_tar() {
    arch_repo
    local o=../out.tar
    v archive -o $o HEAD; arch_snap $o
    v archive -o $o v1; arch_snap $o
    v archive -o $o light; arch_snap $o
    v archive -o $o HEAD~1; arch_snap $o
    v archive -o $o --prefix=pre/ HEAD; arch_snap $o
    v archive -o $o --prefix=a/b/ HEAD; arch_snap $o
    v archive -o $o --prefix=pre- HEAD; arch_snap $o
    v archive -o $o HEAD docs README; arch_snap $o
    v archive -o $o HEAD src/deep; arch_snap $o
    v archive -o $o HEAD 'src/*.c' README; arch_snap $o
    v archive -o $o HEAD 'docs/*.txt'; arch_snap $o
    v archive -o $o HEAD ':(exclude)docs' ':!src'; arch_snap $o
    v archive -o $o --prefix=p/ HEAD src/deep empty; arch_snap $o
    v archive -o $o --mtime=1000000000 HEAD:docs; arch_snap $o
    v archive -o $o --mtime=1000000000 HEAD^{tree}; arch_snap $o
    v archive -v -o $o HEAD docs
    v archive -l
    (cd docs && v archive -o ../$o HEAD; arch_snap ../$o)
    v archive HEAD nosuch
    v archive HEAD docs nosuch
    v archive --format=rar HEAD
    v archive nosuchrev
    v archive
    v archive --bogus HEAD
}

s_archive_targz() {
    arch_repo
    v archive -o ../a.tar.gz HEAD; arch_snap ../a.tar.gz
    v archive -o ../b.tgz --prefix=x/ HEAD docs; arch_snap ../b.tgz
    v archive --format=tgz -9 -o ../c.out HEAD; arch_snap ../c.out.tgz 2>/dev/null; cp ../c.out ../c.out.tgz; arch_snap ../c.out.tgz
    v archive --format=tar.gz -1 -o ../d.out HEAD; cp ../d.out ../d.out.tgz; arch_snap ../d.out.tgz
    v archive --format=tar.gz -0 -o ../e.out HEAD; cp ../e.out ../e.out.tgz; arch_snap ../e.out.tgz
}

s_archive_zip() {
    arch_repo
    v archive -o ../a.zip HEAD; arch_snap ../a.zip
    v archive -o ../b.zip --prefix=top/ v1; arch_snap ../b.zip
    v archive --format=zip -o ../c.zip HEAD docs README; arch_snap ../c.zip
    v archive --format=zip -9 -o ../d.zip HEAD; arch_snap ../d.zip
    v archive --format=zip -1 -o ../e.zip HEAD; arch_snap ../e.zip
    # Stored entries are plain bytes, so the whole file has to match.
    v archive --format=zip -0 -o ../f.zip HEAD; (cd .. && sha1sum f.zip) >>"$TRANSCRIPT"
    v archive --format=zip -0 -o ../g.zip --prefix=p/ HEAD src README; (cd .. && sha1sum g.zip) >>"$TRANSCRIPT"
}

s_archive_add_file() {
    arch_repo
    printf 'extra\n' >../extra.txt
    printf '#!/bin/sh\necho x\n' >../tool.sh; chmod +x ../tool.sh
    v archive -o ../a.tar --add-file=../extra.txt HEAD src; arch_snap ../a.tar
    v archive -o ../b.tar --prefix=a/ --add-file=../extra.txt --prefix=b/ --add-file=../tool.sh HEAD README; arch_snap ../b.tar
    v archive -o ../c.zip --prefix=z/ --add-file ../extra.txt --add-file ../tool.sh HEAD README; arch_snap ../c.zip
    v archive -o ../d.tar --add-file=../nosuch HEAD
}

s_submodule_status() {
    v init -q lib; (cd lib && tick && echo l >l.txt && v add l.txt && v commit -q -m l1 && v tag v1 && tick && echo l2 >>l.txt && v commit -q -am l2)
    v init -q top; cd top
    tick; echo t >t; v add t; v commit -q -m t1
    v submodule
    v submodule status
    g -c protocol.file.allow=always submodule add -q ../lib vendor/lib
    g -c protocol.file.allow=always submodule add -q ../lib other
    tick; v commit -q -m "add submodules"
    v submodule
    v submodule status
    v submodule status --cached
    v submodule status --recursive
    v submodule -q status
    v submodule status --quiet
    v submodule status vendor/lib
    v submodule status other vendor
    v submodule status nosuch
    v submodule status t
    # A submodule that moved on, and one that is on an older commit.
    (cd vendor/lib && tick && echo l3 >>l.txt && v commit -q -am l3)
    (cd other && v checkout -q --detach v1)
    v submodule status
    v submodule status --cached
    # Tags of every kind, and none.
    (cd vendor/lib && v tag light && tick && v tag -a -m annotated ann HEAD~1)
    v submodule status
    (cd vendor/lib && v checkout -q --detach HEAD~2)
    v submodule status
    (cd other && v checkout -q main && v tag -d v1 >/dev/null)
    v submodule status
    (cd other && v checkout -q --detach HEAD~1)
    v submodule status
    # A tag that comes after the commit and none before it: `describe --contains` names it.
    (cd other && v checkout -q main && tick && echo l4 >>l.txt && v commit -q -am l4 && tick && v tag -a -m later later && v checkout -q --detach HEAD~2)
    v submodule status
    (cd other && tick && v tag -a -m nearer nearer HEAD~1 && v checkout -q --detach HEAD~1)
    v submodule status
    # Not checked out.
    g clone -q . ../clone
    (cd ../clone && v submodule status)
    (cd ../clone && v submodule status --cached)
    (cd ../clone && v submodule status other)
    # Conflicted.
    g update-index --force-remove other
    printf '160000 1111111111111111111111111111111111111111 1\tother\n160000 2222222222222222222222222222222222222222 2\tother\n160000 3333333333333333333333333333333333333333 3\tother\n' | g update-index --index-info
    v submodule status
    # Without .gitmodules.
    g read-tree HEAD
    mv .gitmodules ../gitmodules.bak
    v submodule status
    v submodule status vendor/lib
    v submodule status --quiet
    # Without it anywhere but in HEAD.
    g rm -q --cached .gitmodules
    v submodule status
    v submodule status vendor/lib
    v submodule status --quiet
    # Without a mapping for one of the paths.
    g reset -q --hard
    g config -f .gitmodules --remove-section submodule.other
    v submodule status
    v submodule status vendor/lib
    v submodule status --quiet
}

s_describe_all() {
    v init -q repo; cd repo
    tick; echo 1 >f; v add f; v commit -q -m c1
    v tag -a -m "tag one" v1
    tick; echo 2 >f; v commit -q -am c2
    v branch side
    v tag light
    tick; echo 3 >f; v commit -q -am c3
    g update-ref refs/remotes/origin/main HEAD
    tick; echo 4 >f; v commit -q -am c4
    v describe --all
    v describe --all HEAD~1
    v describe --all HEAD~2
    v describe --all HEAD~3
    v describe --all --long HEAD~1
    v describe --all --tags
    v describe --all --always HEAD
    v describe --all --abbrev=10
    v describe --all --exact-match HEAD
    v describe --all --exact-match HEAD~1
    v checkout -q --detach HEAD~1
    v describe --all
    v checkout -q main
    v tag -d light
    v describe --all HEAD~1
    v describe --all --tags HEAD~1
}

# The `.git` file of every given work tree directory, as the transcript sees it.
wt_gitfiles() {
    local d
    for d in "$@"; do
        echo "## gitfile $d" >>"$TRANSCRIPT"
        if [ -e "$d/.git" ]; then sed "s#$ROOT#<ROOT>#g" "$d/.git" >>"$TRANSCRIPT"; else echo "(none)" >>"$TRANSCRIPT"; fi
    done
}

# Reproduces on the base commit: `worktree move` was refused by name there.
s_worktree_move() {
    v init -q repo; cd repo
    tick; printf 'a\nb\n' >f; v add f; v commit -q -m one
    v worktree add -q ../a
    v worktree add -q ../b
    v worktree add -q ../c
    v worktree add -q ../d
    v worktree lock ../b --reason "in use"
    v worktree lock ../c
    echo y >../a/untracked
    mkdir -p ../emptydir ../fulldir/a ../nest
    echo q >../fulldir/a/z
    echo q >../afile
    v worktree move
    v worktree move ../a
    v worktree move ../a ../x ../y
    v worktree move --bogus ../a ../x
    v worktree move ../nosuch ../x
    v worktree move . ../x
    v worktree move ../a ../afile
    v worktree move ../a ../fulldir
    v worktree move ../b ../afile
    v worktree move ../b ../bn
    v worktree move ../c ../cn
    v worktree move -f ../b ../bn
    v worktree move ../a ../missing/parent/x
    wt_snap
    v worktree move ../a ../emptydir
    v worktree move ../b ../bn -f -f
    v worktree move ../c ../nest
    wt_snap
    wt_gitfiles ../emptydir/a ../bn ../nest/c ../d
    v worktree list
    v worktree list --porcelain
    v worktree move ../d ../dn/
    v worktree move d ../dm
    v worktree unlock ../c
    v worktree move ../c ../nest
    (cd ../nest/c && v worktree move . "$ROOT/fromself"; v status -sb)
    (cd ../fromself && v worktree move ../fromself ../c2; v worktree list)
    wt_gitfiles ../c2
    v worktree list
    rm ../dn/.git
    v worktree move ../dn ../d2
    echo "gitdir: ../elsewhere" >../dn/.git
    v worktree move ../dn ../d2
    echo "gitdir: $PWD/.git/worktrees/b" >../dn/.git
    v worktree move ../dn ../d2
    v worktree list
    wt_snap
    (cd ../emptydir/a && v log --oneline && v status -sb)
}

# Reproduces on the base commit: `worktree repair` was refused by name there.
s_worktree_repair() {
    v init -q repo; cd repo
    tick; printf 'a\nb\n' >f; v add f; v commit -q -m one
    v worktree add -q ../a
    v worktree add -q ../b
    v worktree add -q ../c
    v worktree add -q ../gone
    v worktree repair
    v worktree repair ../a ../b
    v worktree repair --bogus
    mv ../a ../a2
    mv ../b ../b2
    v worktree list
    v worktree repair
    wt_snap
    v worktree repair ../a2
    wt_snap
    v worktree repair ../b2 ../a2
    wt_snap
    v worktree list
    echo "--- .git file damaged"
    echo "gitdir: ../nowhere" >../c/.git
    v worktree repair
    wt_gitfiles ../c
    rm ../c/.git
    v worktree repair ../c
    wt_gitfiles ../c
    v worktree repair
    wt_gitfiles ../c
    echo "--- bad arguments"
    mkdir ../plain
    echo x >../afile
    v worktree repair ../plain
    v worktree repair ../afile
    v worktree repair ../nosuch
    rm -rf ../gone
    v worktree repair
    echo "--- from a linked work tree"
    mv ../c ../c2
    (cd ../a2 && v worktree repair ../c2 && v worktree list)
    wt_snap
    wt_gitfiles ../a2 ../b2 ../c2
    v fsck
}

# Moving the main repository breaks every `.git` file; repairing from it restores them.
s_worktree_repair_main() {
    v init -q repo; cd repo
    tick; printf 'a\nb\n' >f; v add f; v commit -q -m one
    v worktree add -q ../a
    v worktree add -q ../b
    cd ..; mv repo moved; cd moved
    v worktree list
    (cd ../a && v status -sb)
    v worktree repair
    wt_snap
    wt_gitfiles ../a ../b
    v worktree list
    (cd ../a && v status -sb && v log --oneline)
    echo "--- both main and a linked tree moved"
    cd ..; mv moved moved2; mv a a2; cd moved2
    v worktree repair
    wt_gitfiles ../a2 ../b
    v worktree repair ../a2
    v worktree list
    wt_snap
    wt_gitfiles ../a2 ../b
    (cd ../a2 && v status -sb)
    v fsck
}

# A superproject whose submodules are all registered in `.gitmodules` but not yet in `.git/config`:
# `lib` (url `../lib`), `other` (url `./rel/`), `cmd` (update = merge), `sub/deep/x` (url `../../up`).
# Built with real git; the clones are what the commands under test work on.
sm_fixture() {
    g init -q lib; (cd lib && tick && echo l >l.txt && g add l.txt && g commit -q -m l1)
    g init -q top; cd top
    tick; echo t >t; g add t; g commit -q -m t1
    g -c protocol.file.allow=always submodule add -q ../lib vendor/lib
    g -c protocol.file.allow=always submodule add -q ../lib other
    g -c protocol.file.allow=always submodule add -q ../lib sub/deep/x
    g -c protocol.file.allow=always submodule add -q ../lib cmd
    g config -f .gitmodules submodule.cmd.update merge
    g config -f .gitmodules submodule.other.url ./rel/
    g config -f .gitmodules submodule.sub/deep/x.url ../../up
    g add .gitmodules; tick; g commit -q -m "add submodules"
    cd ..
}

s_submodule_init() {
    sm_fixture
    g clone -q top clone; g clone -q top clone2; g clone -q top clone3; g clone -q top clone4
    cd clone
    v submodule init nosuch
    v submodule init --bogus
    v submodule init vendor/lib
    v submodule init vendor/lib
    v submodule init -q other
    sed "s#$ROOT#<ROOT>#g" .git/config >>"$TRANSCRIPT"
    (cd sub && v submodule init)
    g config --unset submodule.vendor/lib.url; g config --unset submodule.vendor/lib.active
    g config --unset submodule.cmd.update
    v submodule init -- vendor
    v submodule init
    g config --unset submodule.other.active
    v submodule init other
    cd ../clone2
    g remote remove origin
    v submodule init
    cd ../clone3
    g config submodule.active .
    g config submodule.other.active false
    v submodule init
    v submodule --quiet init
    cd ../clone4
    g update-index --add --cacheinfo 160000,"$(g -C ../lib rev-parse HEAD)",ghost
    v submodule init other
    v submodule init ghost
    v submodule init
    g config --unset submodule.vendor/lib.url
    g config -f .gitmodules submodule.cmd.update '!run'
    v submodule init vendor/lib
}

# `top` has `vendor/lib` and `other`; `lib` has its own submodule `in` (from `inner`). `clone` has
# them all checked out and registered (made with real git), with `.gitmodules` urls edited afterwards.
sm_nested_fixture() {
    g init -q inner; (cd inner && tick && echo i >i.txt && g add i.txt && g commit -q -m i1)
    g init -q lib; (cd lib && tick && echo l >l.txt && g add l.txt && g commit -q -m l1 &&
        g -c protocol.file.allow=always submodule add -q ../inner in && tick && g commit -q -m "add in")
    g init -q top; cd top
    tick; echo t >t; g add t; g commit -q -m t1
    g -c protocol.file.allow=always submodule add -q ../lib vendor/lib
    g -c protocol.file.allow=always submodule add -q ../lib other
    tick; g commit -q -m "add submodules"
    cd ..
    g -c protocol.file.allow=always clone -q --recurse-submodules top clone
}

s_submodule_sync() {
    sm_nested_fixture
    cd clone
    g config -f .gitmodules submodule.vendor/lib.url ../lib2
    g config -f .gitmodules submodule.other.url ../lib3
    g config -f vendor/lib/.gitmodules submodule.in.url ../inner2
    g config --unset submodule.other.active
    v submodule sync nosuch
    v submodule sync --bogus
    v submodule sync vendor/lib
    v submodule sync -q other
    v submodule sync
    g config remote.origin.url ../top
    v submodule sync
    v submodule sync --recursive
    v submodule sync --recursive -q
    g config --unset remote.origin.url
    v submodule sync --recursive vendor
    g -C vendor/lib remote remove origin
    v submodule sync vendor/lib
    (cd vendor && v submodule sync --recursive)
    g config --unset submodule.vendor/lib.url
    g config submodule.vendor/lib.active false
    v submodule sync
    cat .git/modules/vendor/lib/config .git/modules/vendor/lib/modules/in/config | sed "s#$ROOT#<ROOT>#g" >>"$TRANSCRIPT"
}

# What `submodule add` leaves in `.git/modules` (config, HEAD, refs, logs, packed-refs) and the
# `.git` files of the given submodule work trees.
sm_snap() {
    {
        echo "## modules"
        if [ -d .git/modules ]; then
            (cd .git/modules && find . -type f \( -name config -o -name HEAD -o -name packed-refs -o -path '*/refs/*' -o -path '*/logs/*' \) | sort | while read -r f; do
                echo "# $f"; sed "s#$ROOT#<ROOT>#g" "$f"
            done)
        fi
        local d
        for d in "$@"; do echo "## gitfile $d"; cat "$d/.git" 2>&1; done
    } >>"$TRANSCRIPT"
}

s_submodule_add() {
    g config --global protocol.file.allow always
    g init -q lib; (cd lib && tick && echo l >l.txt && g add l.txt && g commit -q -m l1 && g branch dev &&
        tick && echo l2 >>l.txt && g commit -q -a -m l2 && g checkout -q dev && tick && echo d >d.txt &&
        g add d.txt && g commit -q -m d1 && g checkout -q main)
    g init -q top; cd top
    tick; echo t >t; g add t; g commit -q -m t1
    printf 'ign\n' >.gitignore; g add .gitignore; tick; g commit -q -m ign
    v submodule add
    v submodule add --bogus ../lib x
    v submodule add a b c
    v submodule add ../lib vendor/lib
    sm_snap vendor/lib
    v submodule add ../lib vendor/lib
    v submodule add ../lib
    v submodule add -b dev --name mylib ../lib dev/lib2
    v submodule add ../lib x --name n2
    sm_snap lib dev/lib2
    v submodule add -q ../lib q
    v submodule add ../nonexist bad
    mkdir exist; echo x >exist/x; v submodule add ../lib exist
    mkdir exist2; v submodule add ../lib exist2
    v submodule add ../lib t
    v submodule add ../lib ign
    v submodule add -f ../lib ign
    v submodule add ../lib.git
    v submodule add ../lib/ tr/
    v submodule add ../lib ./dotslash
    v submodule add ../lib "a b"
    v submodule add ../lib ""
    v submodule add ../lib .
    v submodule add ../lib ../outside
    v submodule add lib2/ zz
    echo "# c" >>.gitmodules
    v submodule add ../lib more
    mkdir sub; (cd sub && v submodule add ../../lib x/y)
    g init -q inplace; (cd inplace && tick && echo i >i.txt && g add i.txt && g commit -q -m i1)
    v submodule add ../lib inplace
    g rm -q -f --cached q; rm -rf q
    v submodule add ../lib q
    v submodule add -b nosuch ../lib nb
    sm_snap vendor/lib q tr
    v status -s
    v submodule status
    v ls-files -s
    sed "s#$ROOT#<ROOT>#g" .gitmodules >>"$TRANSCRIPT"
    v fsck
    rm -f inplace/.git/index
}

# Absolute and file:// urls end up in `.gitmodules`, so the blob id depends on where the scenario
# runs; only the transcript is compared.
s_submodule_add_urls() {
    g config --global protocol.file.allow always
    g init -q lib; (cd lib && tick && echo l >l.txt && g add l.txt && g commit -q -m l1)
    g init -q top; cd top
    tick; echo t >t; g add t; g commit -q -m t1
    v submodule add "$ROOT/lib" abs
    v submodule add "file://$ROOT/lib" fu
    v submodule add -q "$ROOT/lib/" abs2/
    v submodule add "$ROOT/nonexist" bad
    v submodule add "file://$ROOT/nonexist" bad2
    v submodule add -- ../lib dashes
    v ls-files
    sed "s#$ROOT#<ROOT>#g" .gitmodules .git/config >>"$TRANSCRIPT"
    sm_snap abs fu abs2
    v submodule status
}

# `top` (vendor/lib, other) whose submodules each have a nested submodule `in`; nothing is cloned.
sm_nested_top() {
    g config --global protocol.file.allow always
    g init -q inner; (cd inner && tick && echo i >i.txt && g add i.txt && g commit -q -m i1)
    g init -q lib; (cd lib && tick && echo l >l.txt && g add l.txt && g commit -q -m l1 &&
        g submodule add -q ../inner in && tick && g commit -q -m "add in")
    g init -q top; cd top
    tick; echo t >t; g add t; g commit -q -m t1
    g submodule add -q ../lib vendor/lib
    g submodule add -q ../lib other
    tick; g commit -q -m "add submodules"
    cd ..
}

# Move `lib` forward and point `top` at it (top's own checkouts are made by real git).
sm_bump() {
    (cd "$ROOT/lib" && tick && echo "$1" >>l.txt && g commit -q -a -m "$1")
    (cd "$ROOT/top" && g -C vendor/lib pull -q && g add vendor/lib && tick && g commit -q -m "bump $1")
    (cd "$ROOT/clone" && g pull -q --no-recurse-submodules)
}

s_submodule_update() {
    sm_nested_top
    g clone -q top clone
    cd clone
    v submodule update
    v submodule update vendor/lib
    v submodule update --init vendor/lib
    sm_snap vendor/lib
    v submodule update --init
    sm_snap other
    v submodule update
    v submodule status
    (cd vendor/lib && v status -sb && v log --oneline)
    sm_bump two
    v submodule update vendor/lib
    v submodule status
    sm_bump three
    v submodule update -N vendor/lib
    v submodule update -q vendor/lib
    sm_snap vendor/lib other
    echo dirty >>vendor/lib/l.txt
    sm_bump four
    v submodule update vendor/lib
    v submodule update -f vendor/lib
    v submodule update -f vendor/lib
    v submodule status
    (cd other && g checkout -q main)
    v submodule update other
    (cd other && v branch --show-current)
    rm -rf other
    v submodule update other
    ls other >>"$TRANSCRIPT"
    v submodule update nosuch
    v submodule update --bogus
    g config submodule.vendor/lib.update none
    sm_bump five
    v submodule update vendor/lib
    v submodule update other
}

s_submodule_update_recursive() {
    sm_nested_top
    g clone -q top clone
    cd clone
    v submodule update --init vendor/lib
    v submodule update vendor/lib
    v submodule update --recursive
    v submodule update --init --recursive
    v submodule status --recursive
    sm_snap vendor/lib other
    (cd vendor/lib && v submodule status && v status -s)
    rm -rf vendor/lib/in
    v submodule update --recursive vendor/lib
    v submodule update --init --recursive -q
    v submodule status --recursive
}

s_submodule_deinit() {
    sm_nested_top
    g clone -q top clone
    g clone -q --recurse-submodules top clone2
    cd clone
    v submodule deinit nosuch
    v submodule deinit --bogus
    v submodule deinit
    v submodule deinit --
    v submodule deinit --all vendor/lib
    v submodule deinit vendor/lib
    cd ../clone2
    v submodule deinit vendor/lib
    sm_snap vendor/lib other
    v submodule deinit vendor/lib
    v submodule status
    echo dirty >>other/l.txt
    v submodule deinit other
    echo u >other/u.txt
    v submodule deinit other
    (cd other && g checkout -q -- l.txt)
    v submodule deinit other
    v submodule deinit -f other
    v submodule status
    sm_snap other
    v submodule update --init other
    (cd other && tick && echo c >>l.txt && g commit -q -a -m c)
    v submodule deinit other
    g add other
    v submodule deinit other
    v submodule deinit -q -f other
    v status -s
    ls -a other >>"$TRANSCRIPT"
    v submodule update --init --recursive
    v submodule deinit --all
    v submodule status
    v submodule update --init
    v submodule deinit -f --all
    v submodule status
    v submodule deinit -q --all
    sed "s#$ROOT#<ROOT>#g" .git/config >>"$TRANSCRIPT"
    sm_snap other vendor/lib
}

s_submodule_foreach() {
    sm_nested_top
    g clone -q --recurse-submodules top clone
    g clone -q top clone2
    cd clone
    v submodule foreach 'echo $name $sm_path $displaypath $sha1 $toplevel; pwd'
    v submodule foreach --recursive 'echo $displaypath; echo $path; echo $toplevel'
    v submodule foreach -q 'echo hi'
    v submodule --quiet foreach 'echo hi'
    v submodule foreach 'echo err >&2'
    v submodule foreach false
    v submodule foreach 'exit 3'
    v submodule foreach
    v submodule foreach --bogus x
    v submodule foreach -- echo a
    v submodule foreach --recursive 'test $displaypath != vendor/lib/in'
    v submodule foreach --recursive 'test $displaypath != other'
    v submodule foreach echo a b
    v submodule foreach 'echo "$@"' x
    v submodule foreach 'echo $GIT_DIR; env | grep -c "^path="; git rev-parse --show-toplevel; git status -sb | head -1'
    (cd other && v submodule foreach 'echo $displaypath $toplevel')
    (cd other && v submodule foreach --recursive 'echo $displaypath')
    mkdir sub; (cd sub && v submodule foreach 'echo $displaypath')
    rm -rf other/in/.git
    v submodule foreach --recursive 'echo $sm_path'
    cd ../clone2
    v submodule foreach 'echo $name'
    v submodule foreach --recursive 'echo $name'
    g submodule update --init -q vendor/lib
    v submodule foreach 'echo $name'
}

# Everything about a bundle file that does not depend on how its pack was compressed: the header
# bytes, and what real git says about it.
bundle_snap() {
    local f
    for f in "$@"; do
        {
            echo "## bundle $f"
            sed -n '1,/^$/p' "$f" | sed "s#$ROOT#<ROOT>#g"
            g bundle list-heads "$f" 2>&1
            g bundle verify "$f" 2>&1 | sed "s#$ROOT#<ROOT>#g"
        } >>"$TRANSCRIPT"
    done
}

s_bundle_roundtrip() {
    g init -q src; cd src
    for i in 1 2 3; do tick; echo "line $i" >>f; echo "g$i" >g$i; g add .; g commit -q -m "c$i"; done
    mkdir d; echo deep >d/x; g add d; tick; g commit -q -m "c4 with dir"
    g tag v1 HEAD~2; tick; g tag -a -m annotated av1 HEAD
    g branch side HEAD~1
    v bundle create ../full.bundle main
    v bundle create ../all.bundle --all
    v bundle create ../range.bundle HEAD~2..main
    v bundle create ../tags.bundle main v1 av1
    v bundle create ../two.bundle side main
    v bundle create ../neg.bundle main ^side
    v bundle create ../dash.bundle main --not side
    v bundle create ../x.bundle
    v bundle create
    v bundle create ../x.bundle nosuch
    v bundle create ../x.bundle HEAD~1
    v bundle create ../x.bundle main..main
    v bundle create --version=3 ../v3.bundle main
    v bundle create --version=4 ../x.bundle main
    v bundle create --version 2 ../v2.bundle main
    v bundle create -q ../quiet.bundle HEAD
    bundle_snap ../full.bundle ../all.bundle ../range.bundle ../tags.bundle ../two.bundle ../neg.bundle ../quiet.bundle ../v3.bundle
    v bundle list-heads ../all.bundle
    v bundle list-heads ../all.bundle refs/heads/main refs/tags/v1
    v bundle list-heads ../all.bundle main
    v bundle list-heads
    v bundle verify ../full.bundle
    v bundle verify -q ../full.bundle
    v bundle verify ../range.bundle
    v bundle verify ../all.bundle
    v bundle verify
    v bundle verify nosuch.bundle
    echo hello >../notbundle
    v bundle verify ../notbundle
    v bundle list-heads ../notbundle
    printf '# v2 git bundle\nzzzz\n\n' >../badhdr
    v bundle verify ../badhdr
    printf '# v3 git bundle\n@bogus=1\n\n' >../badcap
    v bundle verify ../badcap
    v bundle unbundle ../full.bundle
    v bundle bogus
    v bundle
    v bundle unbundle --bogus x
    cd ..
    g init -q empty; cd empty
    v bundle verify ../range.bundle
    v bundle verify ../full.bundle
    v bundle unbundle ../range.bundle
    v bundle unbundle ../full.bundle
    v bundle unbundle ../full.bundle refs/heads/main
    v bundle unbundle ../full.bundle main
    g rev-list --objects --all | sort >>"$TRANSCRIPT"
    vq fsck
    vq cat-file -p main
    cd ..
    g init -q e2; cd e2
    v fetch ../range.bundle main
    v fetch ../full.bundle
    v fetch ../full.bundle main:fromb
    v fetch --no-tags ../all.bundle 'refs/heads/*:refs/remotes/b/*'
    v fetch ../all.bundle 'refs/tags/*:refs/tags/*'
    v fetch ../range.bundle main
    v fetch ../full.bundle nosuch
    v for-each-ref
    v branch -a
    v log --oneline fromb
    vq fsck
    g remote add up ../all.bundle
    v fetch --no-tags up
    v for-each-ref
    cd ..
    g init -q e3; cd e3
    g config remote.b.url ../all.bundle
    g config remote.b.fetch '+refs/heads/*:refs/remotes/b/*'
    v fetch --no-tags b
    v for-each-ref
    v log --oneline b/main
    v fetch --no-tags b
    cd ../src
    tick; echo five >>f; g commit -q -a -m c5
    v bundle create ../inc.bundle main ^av1
    bundle_snap ../inc.bundle
    cd ../e3
    v fetch ../inc.bundle main:inc
    v log --oneline inc
    vq fsck
    cd ..
    g init -q e4; cd e4
    vg fetch --no-tags ../all.bundle 'refs/heads/*:refs/remotes/b/*'
    vg fetch ../all.bundle 'refs/tags/*:refs/tags/*'
    vg bundle unbundle ../two.bundle
    vg fsck --strict
    vg for-each-ref
    vg log --oneline --all
}

# `clone` from a bundle file: the directory name comes from the file name (only `.bundle` is
# stripped from a regular file), the bundle's refs become the remote-tracking branches, and a file that
# is not a bundle is refused.
s_bundle_clone() {
    g init -q src; cd src
    for i in 1 2 3; do tick; echo "line $i" >>f; g add .; g commit -q -m "c$i"; done
    g tag v1 HEAD~1; g branch side HEAD~1
    v bundle create ../all.bundle --all
    v bundle create ../nohead.bundle side
    cd ..
    cp all.bundle odd.git
    echo hello >notbundle
    v clone all.bundle
    (cd all && v for-each-ref && v branch -a && v log --oneline && v config --get remote.origin.url | sed "s#$ROOT#<ROOT>#g" && v config --get branch.main.remote && v status -sb && vq fsck)
    v clone all.bundle named
    (cd named && v log --oneline side && v rev-parse HEAD)
    v clone --bare all.bundle
    (cd all.git && v for-each-ref && v symbolic-ref HEAD)
    v clone -b side all.bundle onside
    (cd onside && v status -sb && v log --oneline)
    v clone -b nosuch all.bundle nob
    ls nob 2>&1 | sed "s#$ROOT#<ROOT>#g" >>"$TRANSCRIPT"
    v clone odd.git
    v clone -o up all.bundle upstream
    (cd upstream && v branch -r && v config --get remote.up.fetch)
    v clone nohead.bundle nohead
    (cd nohead && v branch -a && v status -sb)
    v clone notbundle nb
    v clone nosuch.bundle nsb
    v clone ./all.bundle dotted
    (cd dotted && v log --oneline -1)
}

s_clone_bundle_uri() {
    g init -q src; cd src
    for i in 1 2; do tick; echo "line $i" >>f; g add .; g commit -q -m "c$i"; done
    g tag v1
    v bundle create ../b.bundle --all
    tick; echo more >>f; g commit -q -a -m c3
    cd ..
    echo junk >junk.bundle
    v clone --bundle-uri=$ROOT/b.bundle src seeded
    (cd seeded && v for-each-ref && v log --oneline && v config --get remote.origin.url | sed "s#$ROOT#<ROOT>#g" && vq fsck)
    v clone --bundle-uri=file://$ROOT/b.bundle src viafile
    (cd viafile && v for-each-ref)
    v clone --bundle-uri=./b.bundle --bare src bare.git
    (cd bare.git && v for-each-ref)
    v clone --bundle-uri=$ROOT/nosuch.bundle src missing
    (cd missing && v for-each-ref && v log --oneline -1)
    v clone --bundle-uri=https://x.invalid/b src remote
    (cd remote && v for-each-ref | head -1)
}

s_clone_filter() {
    g init -q src; cd src
    for i in 1 2; do tick; echo "line $i" >>f; g add .; g commit -q -m "c$i"; done
    cd ..
    v clone --filter=blob:none src local
    (cd local && v config core.repositoryformatversion && v config remote.origin.promisor && v config remote.origin.partialclonefilter && v config remote.origin.url | sed "s#$ROOT#<ROOT>#g" && v log --oneline && vq fsck)
    v clone --filter=blob:none file://$ROOT/src viafile
    (cd viafile && v config core.repositoryformatversion && v config remote.origin.promisor && v config remote.origin.partialclonefilter && v config remote.origin.url | sed "s#$ROOT#<ROOT>#g" && v status -sb)
    v clone --filter=tree:0 --bare file://$ROOT/src bare.git
    (cd bare.git && v config core.repositoryformatversion && v config remote.origin.promisor && v config remote.origin.partialclonefilter && v config remote.origin.url | sed "s#$ROOT#<ROOT>#g")
    v clone --filter blob:limit=1k file://$ROOT/src limit
    (cd limit && v config remote.origin.partialclonefilter)
    v clone --filter=bogus src bogus
    v clone --filter=blob:limit=zz file://$ROOT/src bogus2
    ls bogus bogus2 2>&1 | sed "s#$ROOT#<ROOT>#g" >>"$TRANSCRIPT"
}

s_ls_remote() {
    g init -q src; cd src
    for i in 1 2 3; do tick; echo "line $i" >>f; g add .; g commit -q -m "c$i"; done
    g tag light HEAD~1; tick; g tag -a ann -m annotated; g branch side HEAD~1
    v bundle create ../all.bundle --all
    cd ..
    echo hello >notbundle
    v ls-remote src
    v ls-remote --heads src
    v ls-remote -t src
    v ls-remote --refs src
    v ls-remote --heads --tags src
    v ls-remote src 'ma*' refs/tags/ann
    v ls-remote src HEAD
    v ls-remote --symref src HEAD
    v ls-remote --symref src
    v ls-remote src nothing
    v ls-remote --exit-code src nothing
    v ls-remote --exit-code src side
    v ls-remote -q src nothing
    v ls-remote --get-url src
    v ls-remote file://$ROOT/src
    v ls-remote all.bundle
    v ls-remote --refs all.bundle
    v ls-remote nosuch
    v ls-remote notbundle
    cd src
    v remote add o ../src
    v ls-remote o
    v ls-remote --get-url o
    v ls-remote --heads o 'si*'
    v ls-remote
    v ls-remote -q
    v ls-remote ../src
}

# The four small divergences that were left open after the worktree block.
s_reset_current_branch() {
    v init -q repo; cd repo
    tick; echo a >a; v add a; v commit -q -m one
    tick; echo b >>a; v commit -q -a -m two
    g branch other HEAD~1
    v switch -C main
    v switch -C main HEAD~1
    v switch -C main main
    v checkout -B main
    v checkout -B main HEAD
    v checkout -B other
    v checkout -B brand
    v checkout -B brand
    v checkout -B main other
    v switch -C brand
    v switch -C brand brand
    v switch -C other main
    v checkout -B other
    v branch -v
    v reflog -n 20
    v checkout -B nosuch/../x HEAD
}

s_cat_file_index() {
    v init -q repo; cd repo
    tick; echo a >a; mkdir d; echo b >d/b; v add .; v commit -q -m one
    tick; echo a2 >>a; v add a
    echo untracked >u
    v cat-file -p :a
    v cat-file -p :0:a
    v cat-file -p :d/b
    v cat-file -t :a
    v cat-file -s :a
    v cat-file -e :a
    v cat-file -p :1:a
    v cat-file -p :nosuch
    v cat-file -p :u
    v cat-file -e :nosuch
    v cat-file -p HEAD:a
    (cd d && v cat-file -p :./b && v cat-file -p :b && v cat-file -p :../a && v cat-file -p :d/b)
    # A conflict: stages 1-3 exist and stage 0 does not.
    g checkout -q -b side HEAD~0; tick; echo side >>a; g commit -q -a -m side
    g checkout -q main; tick; echo main >>a; g commit -q -a -m mainchange
    g merge side >/dev/null 2>&1
    v cat-file -p :a
    v cat-file -p :1:a
    v cat-file -p :2:a
    v cat-file -p :3:a
    v cat-file -p :4:a
    v cat-file -e :a
    v ls-files -s
}

s_reflog_index_edges() {
    v init -q repo; cd repo
    tick; echo a >a; v add a; v commit -q -m one
    tick; echo b >>a; v commit -q -a -m two
    v log -1 --oneline HEAD@{1}
    v log -1 --oneline main@{1}
    v log -1 --oneline HEAD@{5}
    v log -1 --oneline main@{5}
    v log -1 --oneline HEAD@{0}
    v log HEAD@{5}..HEAD
    v show -s HEAD@{5}
    v show -s HEAD@{1}
    v rev-parse HEAD@{5} main@{5}
    v rev-parse HEAD@{0}
    : >.git/logs/HEAD
    v log -1 --oneline HEAD@{0}
    v log -1 --oneline HEAD@{1}
    v rev-parse HEAD@{0}
    v rev-parse HEAD@{1}
    v show -s HEAD@{1}
    v log HEAD@{1}..HEAD
    : >.git/logs/refs/heads/main
    v rev-parse main@{0}
    v rev-parse main@{1}
    v log -1 --oneline main@{1}
    rm .git/logs/HEAD
    v rev-parse HEAD@{0}
    v rev-parse HEAD@{1}
    v show -s HEAD@{1}
}

s_archive_remote() {
    arch_repo
    cd ..
    g init -q other; cd other
    v archive --remote=../repo HEAD -o ../a.tar; arch_snap ../a.tar
    v archive --remote=../repo v1 -o ../b.tar; arch_snap ../b.tar
    v archive --remote=../repo light --prefix=p/ -o ../c.tar src; arch_snap ../c.tar
    v archive --remote=../repo --format=zip HEAD -o ../d.zip; arch_snap ../d.zip
    vq archive --remote=../repo HEAD~1 -o ../e.tar.gz; ls -l ../e.tar.gz | awk '{print $5}' >>"$TRANSCRIPT"
    v archive --remote "file://$ROOT/repo" HEAD docs -o ../f.tar; arch_snap ../f.tar
    v archive --remote=../repo --exec=git-upload-archive HEAD -o ../g.tar; arch_snap ../g.tar
    v archive --remote=../repo/.git HEAD -o ../i.tar; arch_snap ../i.tar
    v archive --remote=../repo -l
    vq archive --remote=../repo nosuch -o ../x.tar
    vq archive --remote=../repo HEAD nosuchdir -o ../x.tar
    vq archive --remote=../nosuchrepo HEAD -o ../x.tar
    vq archive --remote=../repo/src HEAD -o ../x.tar
    v archive --remote=../repo --exec=/nonexistent HEAD -o ../x.tar
    vq archive --remote=../repo HEAD~1 -o ../y.tar
    v archive --remote=../repo HEAD:src --format=zip -o ../z.zip
}

s_archive_worktree_attributes() {
    arch_repo
    v archive -o ../a.tar HEAD; arch_snap ../a.tar
    printf 'README export-ignore\n' >.gitattributes
    printf 'src/a.c export-ignore\n' >src/.gitattributes
    v archive -o ../b.tar HEAD; arch_snap ../b.tar
    v archive --worktree-attributes -o ../c.tar HEAD; arch_snap ../c.tar
    v archive --worktree-attributes --no-worktree-attributes -o ../d.tar HEAD; arch_snap ../d.tar
    v archive --worktree-attributes -o ../e.tar HEAD~1 docs src; arch_snap ../e.tar
    rm .gitattributes
    v archive --worktree-attributes -o ../f.tar HEAD; arch_snap ../f.tar
    (cd src && v archive --worktree-attributes -o ../g.tar HEAD; arch_snap ../g.tar)
    printf 'deep/ export-ignore\n' >src/.gitattributes
    v archive --worktree-attributes --format=zip -o ../h.zip HEAD; arch_snap ../h.zip
    # A directory-only pattern in the committed attributes drops the whole directory.
    printf 'docs/ export-ignore\nsrc/deep/ export-ignore\n' >.gitattributes
    rm src/.gitattributes
    v add .gitattributes; tick; v commit -q -m attrs
    v archive -o ../i.tar HEAD; arch_snap ../i.tar
}

s_describe_contains() {
    v init -q repo; cd repo
    local i
    for i in 1 2 3 4 5; do tick; echo $i >>f; v add f; v commit -q -m "c$i"; done
    v describe --contains HEAD
    v describe --contains --always HEAD
    v describe --contains HEAD~1
    v tag light HEAD~1
    v describe --contains HEAD~1
    v describe --contains HEAD~3
    v describe --contains --tags HEAD~4
    v describe --contains --all HEAD~2
    v describe --contains --long --exact-match --abbrev=3 HEAD~2
    tick; v tag -a -m ann ann HEAD~1
    v describe --contains HEAD~1
    v describe --contains HEAD~2
    tick; v tag -a -m onlyann annonly HEAD
    v describe --contains HEAD
    v describe --contains --always HEAD~4
    v tag v2 HEAD
    v describe --contains HEAD~3
    v describe --contains HEAD
    v describe --contains nosuch
    # A second, newer tag at the same distance does not take the name; a nearer one does.
    tick; v tag -a -m z zlater HEAD~2
    v describe --contains HEAD~3
    v tag a-nearer HEAD~3
    v describe --contains HEAD~3
    v describe --contains HEAD~4
    # Merges.
    g checkout -q -b side HEAD~3
    tick; echo s >s; v add s; v commit -q -m side1
    tick; echo s2 >s; v commit -q -a -m side2
    g checkout -q main
    tick; v merge -q --no-ff side -m merge
    tick; v tag -a -m m mergetag
    v describe --contains side
    v describe --contains side~1
    v describe --contains HEAD~1
    v describe --contains HEAD~2
    v describe --contains HEAD^2~1
    v describe --contains --all side~1
    v describe --contains --all HEAD~1
    tick; echo six >>f; v commit -q -a -m c6
    v describe --contains HEAD
    v describe --contains --all HEAD
    v describe --contains --all HEAD~1
    v branch old HEAD~3
    v describe --contains --all old
    v describe --contains old~1
    v describe --contains HEAD~4
    v describe
    v describe --tags
}

s_submodule_set() {
    sm_nested_top
    g clone -q --recurse-submodules top clone
    cd clone
    printf '# kept comment\n' >>.gitmodules
    v submodule set-branch -b dev vendor/lib
    sed "s#$ROOT#<ROOT>#g" .gitmodules >>"$TRANSCRIPT"
    v status -s
    v submodule set-branch --branch=dev2 other
    v submodule set-branch -q -b dev3 other
    v submodule set-branch -d vendor/lib
    v submodule set-branch -d vendor/lib
    v submodule set-branch vendor/lib
    v submodule set-branch -b dev
    v submodule set-branch -b dev -d vendor/lib
    v submodule set-branch -b dev nosuch
    v submodule set-branch -b dev t
    v submodule set-branch -b dev -- vendor/lib other
    v submodule set-branch -b
    v submodule set-branch --bogus x
    sed "s#$ROOT#<ROOT>#g" .gitmodules >>"$TRANSCRIPT"
    v submodule set-url vendor/lib ../lib2
    sed "s#$ROOT#<ROOT>#g" .gitmodules .git/config >>"$TRANSCRIPT"
    v submodule set-url -q other ../lib3
    v submodule set-url vendor/lib
    v submodule set-url nosuch ../x
    v submodule set-url other /abs/path
    v submodule set-url -- other ./rel
    v submodule set-url --bogus a b
    g config --unset submodule.other.active
    g config --unset submodule.other.url
    v submodule set-url other ../lib4
    sed "s#$ROOT#<ROOT>#g" .gitmodules .git/config >>"$TRANSCRIPT"
    sm_snap vendor/lib other
    v submodule status
    mv .gitmodules ../gm.bak
    v submodule set-url vendor/lib ../zz
    v submodule set-branch -b dev vendor/lib
}

s_submodule_absorb() {
    g init -q inner; (cd inner && tick && echo i >i.txt && g add i.txt && g commit -q -m i1)
    g init -q lib; (cd lib && tick && echo l >l.txt && g add l.txt && g commit -q -m l1 &&
        g clone -q ../inner in && g add in 2>/dev/null && g config -f .gitmodules submodule.in.path in &&
        g config -f .gitmodules submodule.in.url ../inner && g add .gitmodules && tick && g commit -q -m "add in")
    g init -q top; cd top
    tick; echo t >t; g add t; g commit -q -m t1
    g clone -q ../lib vendor/lib; g clone -q ../inner vendor/lib/in
    g clone -q ../lib other; g clone -q ../lib spare
    g add vendor/lib other spare 2>/dev/null
    g config -f .gitmodules submodule.vendor/lib.path vendor/lib
    g config -f .gitmodules submodule.vendor/lib.url ../lib
    g config -f .gitmodules submodule.spare.path spare
    g config -f .gitmodules submodule.spare.url ../lib
    g add .gitmodules; tick; g commit -q -m subs
    v submodule absorbgitdirs nosuch
    v submodule absorbgitdirs --bogus
    v submodule absorbgitdirs -q
    v submodule absorbgitdirs t
    v submodule absorbgitdirs other
    v submodule absorbgitdirs
    g rm -q --cached other
    v submodule absorbgitdirs spare
    sm_snap spare
    v submodule absorbgitdirs spare
    v submodule absorbgitdirs
    sm_snap vendor/lib vendor/lib/in
    v submodule absorbgitdirs
    v submodule status
    v status -s
    (cd vendor/lib && v status -sb && v log --oneline)
    (cd vendor/lib/in && v status -sb)
    mkdir sub
    (cd sub && v submodule absorbgitdirs ../spare)
    rm -rf other
}

s_submodule_summary() {
    g config --global protocol.file.allow always
    g init -q lib; (cd lib && tick && echo 1 >f && g add f && g commit -q -m "l1 first" && tick && echo 2 >>f && g commit -q -a -m "l2 second")
    g init -q top; cd top
    tick; echo t >t; g add t; g commit -q -m t1
    g submodule add -q ../lib vendor/lib; g submodule add -q ../lib other
    tick; g commit -q -m subs
    g clone -q . ../clone
    v submodule summary
    (cd vendor/lib && for m in "l3 third" "l4 fourth" "l5 fifth"; do tick; echo "$m" >>f; g commit -q -a -m "$m"; done)
    v submodule summary
    v submodule summary --files
    v submodule summary --cached
    v submodule summary --cached --files
    g add vendor/lib
    v submodule summary
    v submodule summary --cached
    v submodule summary --files
    v submodule summary HEAD
    v submodule summary -n 1
    v submodule summary --summary-limit 2
    v submodule summary --summary-limit=0
    v submodule summary -n -1
    v submodule summary -n
    v submodule summary -n x
    v submodule summary vendor/lib
    v submodule summary -- other
    v submodule summary nosuch
    v submodule summary HEAD vendor/lib
    v submodule summary HEAD -- vendor/lib
    v submodule summary -q
    v submodule --quiet summary
    tick; g commit -q -m bump
    v submodule summary HEAD~1
    v submodule summary HEAD~1 HEAD
    v submodule summary --cached HEAD~1
    v submodule summary --cached HEAD~2
    v submodule summary HEAD~2
    (cd other && echo dirty >>f)
    v submodule summary --files
    v submodule summary
    (cd other && g checkout -q -- f && g reset -q --hard HEAD~1)
    v submodule summary --files
    v submodule summary
    (cd vendor/lib && g reset -q --hard HEAD~4)
    v submodule summary --files
    g add vendor/lib
    v submodule summary --cached
    v submodule summary --cached HEAD
    v submodule summary --cached -n 1
    g rm -q --cached other
    v submodule summary --cached
    v submodule summary --files
    # A submodule whose commits are not there, and one that is not checked out.
    g update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,ghost
    v submodule summary --cached
    v submodule summary
    g reset -q
    (cd ../clone && g update-index --cacheinfo 160000,1111111111111111111111111111111111111111,vendor/lib && v submodule summary --cached && v submodule summary --files && v submodule summary)
    mkdir sub; (cd sub && v submodule summary ../vendor/lib)
    # A gitlink replaced by a file and the other way round.
    rm -rf other; echo file >other; g add other
    v submodule summary --cached
    v submodule summary
}

s_symbolic_ref() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v branch dev
    # reads
    v symbolic-ref HEAD
    v symbolic-ref --short HEAD
    v symbolic-ref -q HEAD
    v symbolic-ref --no-short HEAD
    v symbolic-ref refs/heads/main
    v symbolic-ref -q refs/heads/main
    v symbolic-ref --short refs/heads/main
    v symbolic-ref nonsense
    v symbolic-ref -q nonsense
    v symbolic-ref refs/nope
    v symbolic-ref 'bad name'
    v symbolic-ref
    v symbolic-ref a b c
    # write, read back, with and without a reason
    v symbolic-ref HEAD refs/heads/dev
    v symbolic-ref HEAD
    v symbolic-ref --short HEAD
    v symbolic-ref -m switching HEAD refs/heads/main
    v symbolic-ref HEAD refs/heads/main
    v symbolic-ref HEAD foo
    v symbolic-ref HEAD refs/heads/a..b
    v symbolic-ref HEAD refs/tags/t
    v symbolic-ref HEAD refs/heads/main
    # an unborn target reads fine
    v symbolic-ref HEAD refs/heads/unborn
    v symbolic-ref HEAD
    v symbolic-ref --short HEAD
    v symbolic-ref HEAD refs/heads/main
    # other symbolic refs, chains, and a dangling target
    v symbolic-ref refs/heads/alias refs/heads/main
    v symbolic-ref refs/heads/alias
    v symbolic-ref --short refs/heads/alias
    v symbolic-ref refs/heads/alias2 refs/heads/alias
    v symbolic-ref refs/heads/alias2
    v symbolic-ref --recurse refs/heads/alias2
    v symbolic-ref --no-recurse refs/heads/alias2
    v symbolic-ref -m why refs/heads/alias3 refs/heads/dev
    v symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main
    v symbolic-ref refs/remotes/origin/HEAD
    v symbolic-ref --short refs/remotes/origin/HEAD
    v symbolic-ref refs/heads/dangling refs/heads/gone
    v symbolic-ref refs/heads/dangling
    v log --oneline refs/heads/alias
    # a name that is also a tag is shortened with its prefix
    v tag main
    v symbolic-ref --short HEAD
    v tag -d main
    # delete
    v symbolic-ref -d refs/heads/alias
    v symbolic-ref --delete refs/heads/alias
    v symbolic-ref --delete -q refs/heads/alias
    v symbolic-ref --delete refs/heads/dev
    v symbolic-ref --delete refs/heads/nope
    v symbolic-ref --delete HEAD
    v symbolic-ref --delete
    v symbolic-ref --delete a b
    v symbolic-ref --delete refs/remotes/origin/HEAD
    v symbolic-ref --delete refs/heads/alias2
    v symbolic-ref --delete refs/heads/dangling
    v symbolic-ref refs/heads/dev
    v branch
    # detached HEAD
    g checkout -q --detach
    v symbolic-ref HEAD
    v symbolic-ref -q HEAD
    v symbolic-ref --short HEAD
    v symbolic-ref HEAD refs/heads/main
    v symbolic-ref HEAD
}

# `rebase -i` over D, E, G (topic) onto main. `SEQ` is a sed script the sequence editor applies to the
# todo list; `../msg.sh` is the message editor (it prefixes the first line of the message).
rebase_i_helpers() {
    printf '#!/bin/sh\nprintf "reworded %%s\\n" "$(head -n1 "$1")" >"$1"\n' >../msg.sh
    printf '#!/bin/sh\nsed -i -e "$SEQ" "$1"\n' >../seq.sh
    chmod +x ../msg.sh ../seq.sh
}

# Run `rebase -i main` on a fresh topic with the todo edited by `$1`; show what is left behind.
rebase_i_run() {
    [ -d .git/rebase-merge ] && v rebase --abort
    v switch -q topic
    v reset -q --hard orig
    SEQ="$1" GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" GIT_EDITOR="$PWD/../msg.sh" v rebase -i main
    echo "rc=$?"
    v log --format='%H %an %s%n%b' main..topic
    v status
}

s_rebase_i() {
    rebase_repo
    g branch orig topic
    rebase_i_helpers
    # The todo list the editor sees, and a run that changes nothing.
    GIT_SEQUENCE_EDITOR=cat v rebase -i main topic
    echo "rc=$?"
    v log --oneline --graph
    v reflog
    cat .git/ORIG_HEAD
    # Reword, squash, fixup, drop and a deleted line.
    for edit in '1s/^pick/reword/' '2s/^pick/reword/' '2s/^pick/squash/' '3s/^pick/fixup/' '2s/^pick/drop/' '2d' \
        '2s/^pick/squash/;3s/^pick/squash/' '2s/^pick/fixup/;3s/^pick/squash/' '2s/^pick/squash/;3s/^pick/fixup/' \
        '2s/^pick/fixup/;3s/^pick/fixup/' '1s/^pick/reword/;2s/^pick/squash/' '1s/^pick/drop/;2s/^pick/squash/' \
        '2s/^pick/squash/;3s/^pick/drop/' '1,2d' '3s/^pick/drop/;2s/^pick/fixup/' '1,3d' '1s/^pick/p/;2s/^pick/r/;3s/^pick/s/'; do
        rebase_i_run "$edit"
    done
    v reflog
    # Reordering, and a todo that is empty from the start.
    rebase_i_run '1{h;d};3{G}'
    v switch -q topic
    v reset -q --hard orig
    v rebase -i topic
    echo "rc=$?"
    v status
}

s_rebase_i_stops() {
    rebase_repo
    g branch orig topic
    rebase_i_helpers
    # A first command that cannot work stops before anything is applied.
    rebase_i_run '1s/^pick/fixup/'
    v rebase --abort
    v status
    rebase_i_run '1s/^pick/foo/'
    v rebase --abort
    v log --oneline
    # edit: stop, continue untouched, with a staged change, with an unstaged one, after an amend.
    rebase_i_run '2s/^pick/edit/'
    ls .git/rebase-merge
    v rebase --continue
    v log --format='%H %s' main..topic
    rebase_i_run '2s/^pick/edit/'
    echo more >>e
    v rebase --continue
    v add e
    v rebase --continue
    v log --format='%H %s' main..topic
    v reflog
    rebase_i_run '2s/^pick/edit/'
    echo more >>e
    v commit -q -a --amend -m "amended e"
    v rebase --continue
    v log --format='%H %s' main..topic
    rebase_i_run '2s/^pick/edit/'
    echo new >nn; v add nn
    v commit -q -m extra
    echo more >>nn; v add nn
    v rebase --continue
    v rebase --abort
    v log --format='%H %s' main..topic
    # edit on the first command, and a fast-forward edit.
    rebase_i_run '1s/^pick/edit/'
    v rebase --continue
    v reflog
    # --edit-todo: nothing to edit, what the editor sees, a valid edit, a bad one fixed by another
    # edit, an editor that fails, and an emptied list.
    v rebase --edit-todo
    todo_state() { cat .git/rebase-merge/git-rebase-todo; echo "-- backup"; cat .git/rebase-merge/git-rebase-todo.backup; echo "-- end/msgnum"; cat .git/rebase-merge/end .git/rebase-merge/msgnum; v status; }
    rebase_i_run '1s/^pick/edit/'
    GIT_SEQUENCE_EDITOR=cat v rebase --edit-todo
    echo "rc=$?"
    todo_state
    SEQ='2d;1s/^pick/reword/' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase --edit-todo
    echo "rc=$?"
    todo_state
    GIT_SEQUENCE_EDITOR=false v rebase --edit-todo
    echo "rc=$?"
    todo_state
    SEQ='s/^reword/pick/' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase --edit-todo
    SEQ='s/^pick/foo/' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase --edit-todo
    echo "rc=$?"
    todo_state
    v rebase --continue
    echo "rc=$?"
    SEQ='s/^foo/pick/' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase --edit-todo
    echo "rc=$?"
    todo_state
    GIT_EDITOR="$PWD/../msg.sh" v rebase --continue
    echo "rc=$?"
    v log --format='%H %s' main..topic
    rebase_i_run '2s/^pick/edit/'
    SEQ='/^pick/d' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase --edit-todo
    echo "rc=$?"
    v status
    v rebase --continue
    v log --format='%H %s' main..topic
    v reflog
    # A list that was invalid from the start (first command a fixup), fixed with --edit-todo.
    rebase_i_run '1s/^pick/fixup/'
    GIT_SEQUENCE_EDITOR=cat v rebase --edit-todo
    echo "rc=$?"
    todo_state
    SEQ='s/^fixup/pick/' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase --edit-todo
    todo_state
    v rebase --continue
    v log --format='%H %s' main..topic
}

s_rebase_i_conflict() {
    rebase_repo
    g branch orig topic
    rebase_i_helpers
    v switch -q main
    tick; echo "main f" >>f; v commit -q -a -m "main f"
    rebase_i_run '1s/^pick/reword/'
    v diff
    v rebase --continue
    printf 'f\nresolved\n' >f
    v add f
    v rebase --continue
    v log --format='%H %s%n%b' main..topic
    v status
    rebase_i_run 's/^pick/squash/;1s/^squash/pick/'
    v rebase --skip
    v log --format='%H %s%n%b' main..topic
    rebase_i_run '2s/^pick/squash/'
    v rebase --abort
    v log --format='%H %s' main..topic
    cat .git/ORIG_HEAD
    rebase_i_run '3s/^pick/fixup/'
    printf 'f\nresolved\n' >f
    v add f
    v rebase --continue
    v log --format='%H %s%n%b' main..topic
    # reword and edit on the conflicting commit, squash as the third link of a chain.
    rebase_i_run '2s/^pick/reword/'
    printf 'f\nresolved\n' >f
    v add f
    v rebase --continue
    v log --format='%H %s%n%b' main..topic
    rebase_i_run '2s/^pick/edit/'
    printf 'f\nresolved\n' >f
    v add f
    v rebase --continue
    v status
    v rebase --continue
    v log --format='%H %s%n%b' main..topic
    rebase_i_run '2{h;d};3{G;s/^pick/squash/;s/\npick/\nsquash/}'
    v rebase --skip
    v log --format='%H %s%n%b' main..topic
    rebase_i_run '2{h;d};3{G;s/^pick/fixup/;s/\npick/\nsquash/}'
    printf 'f\nresolved\n' >f
    v add f
    v rebase --continue
    v log --format='%H %s%n%b' main..topic
    v reflog
}

# `log` over several revisions: tips on two branches whose dates force one interleaving that is
# neither "left first" nor "right first", ranges mixed with plain revisions, and `--` paths.
s_log_multi() {
    v init -q repo; cd repo
    tick; echo 1 >a; v add a; v commit -q -m base
    v branch left; v branch right
    v switch -q left; tick; echo l1 >l; v add l; v commit -q -m L1
    v switch -q right; tick; echo r1 >r; v add r; v commit -q -m R1
    v switch -q left; tick; echo l2 >>l; v commit -q -a -m L2
    v switch -q right; tick; echo r2 >>r; v commit -q -a -m R2
    tick; echo r3 >>r; v commit -q -a -m R3
    v switch -q left; echo l3 >>l; v commit -q -a -m L3
    v switch -q main; tick; echo m >m; v add m; v commit -q -m M1
    v log --oneline left right
    v log --oneline right left
    v log --oneline left right main
    v log --oneline main left
    v log --oneline main..left right
    v log --oneline left main..right
    v log --oneline ^left right
    v log --oneline right ^left
    v log --oneline left...right right
    v log --oneline left right -- l
    v log --oneline left right -- r
    v log --oneline -n3 left right
    v log --oneline --skip=2 left right
    v log --oneline --reverse left right
    v log --oneline --topo-order left right
    v log --oneline --first-parent left right
    v log --format='%s %p' left right main
    v log --oneline HEAD left
    v log --oneline left left
    v log --oneline left nosuch
    v log --oneline nosuch left
    v log --oneline left right --
    v log --oneline --graph left right
    v log --oneline --all left
    v log --oneline ^left
    v log --oneline ^left ^right
}

# Push refspec forms: the last colon splits, `:` pushes the branches both sides have, `+` forces,
# and a dry run prints the same markers as a real push.
s_push_refspec_forms() {
    g init -q --bare origin.git
    v init -q repo; cd repo
    v remote add origin file://$ROOT/origin.git
    echo a >a; v add a; v commit -q -m one
    v push origin main
    v branch topic; v branch other
    v push origin topic
    v push origin main:feature
    v push origin 'main:topic:z'
    v push origin 'main:refs/heads/a:b'
    v push origin 'x:y:z'
    v push origin 'refs/heads/main:refs/heads/dst:'
    v push origin ':'
    tick; echo b >b; v add b; v commit -q -m two
    v push --dry-run origin :
    v push origin :
    v push --dry-run origin :
    tick; v switch -q topic; echo t >t; v add t; v commit -q -m three
    v push --dry-run origin :
    v push origin :
    v switch -q main
    v push --dry-run origin main:fresh topic:other :feature main
    v push --dry-run origin +topic:main
    v push --dry-run origin topic:main
    v push origin --delete feature
    v push origin :nosuch
    # git fails the whole command line while matching refspecs, before anything is sent: the
    # branch named next to the missing delete must NOT reach the remote (checked with git 2.56).
    v branch good-branch
    v push origin good-branch :nosuch
    v push origin :nosuch good-branch
    v push --dry-run origin good-branch :nosuch
    (cd ../origin.git && v for-each-ref)
    v push origin +:
    (cd ../origin.git && v for-each-ref)
}

# `rebase -i --autosquash`: the todo order (exact subject, commit id, subject prefix, nested
# `fixup! fixup!`, a fixup with no target, `fixup!` without the space), then the resulting history.
# Each fixup adds its own file so that the reorder cannot conflict. `amend!` targets are refused by
# rivet (they need `fixup -C`), so they are not in this differential scenario.
s_rebase_autosquash() {
    v init -q repo; cd repo
    echo base >base; v add base; v commit -q -m base
    v branch work
    tick; echo main >main.txt; v add main.txt; v commit -q -m "main moves"
    v switch -q work
    for n in A B C; do tick; echo $n >$n; v add $n; v commit -q -m "feat $n"; done
    local a; a=$(g rev-parse --short HEAD~2)
    tick; echo 1 >f1; v add f1; v commit -q -m "fixup! feat A"
    tick; echo 2 >f2; v add f2; v commit -q -m "squash! feat B"
    tick; echo 3 >f3; v add f3; v commit -q -m "fixup! fixup! feat A"
    tick; echo 4 >f4; v add f4; v commit -q -m "fixup! $a"
    tick; echo 5 >f5; v add f5; v commit -q -m "fixup! nomatch"
    tick; echo 6 >f6; v add f6; v commit -q -m "fixup! feat"
    tick; echo 7 >f7; v add f7; v commit -q -m "fixup!feat B"
    tick; echo 8 >f8; v add f8; v commit -q -m "squash! squash! feat C"
    g branch orig
    rebase_i_helpers
    export GIT_EDITOR=true
    for flags in "--autosquash" "--no-autosquash" "" "--autosquash --no-autosquash" "--no-autosquash --autosquash"; do
        v switch -q work; v reset -q --hard orig
        echo "== -i $flags"
        GIT_SEQUENCE_EDITOR=cat v rebase -i $flags main
        echo "rc=$?"
        v log --format='%H %s' main..work
        v reflog
        v status
    done
    for flags in "" "--no-autosquash" "--autosquash"; do
        v switch -q work; v reset -q --hard orig
        v config rebase.autoSquash true
        echo "== config true -i $flags"
        GIT_SEQUENCE_EDITOR=cat v rebase -i $flags main
        echo "rc=$?"
        v log --format='%H %s' main..work
        v config --unset rebase.autoSquash
    done
    # A squashed `fixup! …` / `squash! …` subject is commented out of the message, reordered or not.
    for edit in '4s/^pick/squash/' '2s/^pick/squash/;4s/^pick/squash/;5s/^pick/fixup/' '5s/^pick/squash/;6s/^pick/squash/'; do
        v switch -q work; v reset -q --hard orig
        echo "== squash by hand: $edit"
        SEQ="$edit" GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase -i --no-autosquash main
        echo "rc=$?"
        v log --format='%H%n%B' main..work
    done
    # Stop on the first reordered commit: the todo that is left and `status`.
    v switch -q work; v reset -q --hard orig
    GIT_SEQUENCE_EDITOR="sed -i -e 1s/^pick/edit/" v rebase -i --autosquash main
    cat .git/rebase-merge/git-rebase-todo
    v status
    v rebase --abort
}

# `exec` todo lines and `rebase -i --exec`: the generated todo list (one exec per -x, after a whole
# squash/fixup chain), stdout/stderr of the command, a failing command that stops the rebase, and
# `--continue` afterwards.
s_rebase_exec() {
    rebase_repo
    g branch orig topic
    rebase_i_helpers
    GIT_SEQUENCE_EDITOR=cat v rebase -i -x 'echo A' main topic
    echo "rc=$?"
    v log --format='%H %s' main..topic
    v reflog
    v switch -q topic
    v reset -q --hard orig
    GIT_SEQUENCE_EDITOR=cat v rebase -i -x 'echo A' -x 'echo B' main
    echo "rc=$?"
    v switch -q topic
    v reset -q --hard orig
    SEQ='2s/^pick/squash/' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" GIT_EDITOR=true v rebase -i -x 'echo C' main
    echo "rc=$?"
    v log --format='%H %s' main..topic
    # A failing exec stops with the commit applied; --continue goes on.
    rebase_i_run '1a exec echo before; false'
    v status
    v rebase --continue
    echo "rc=$?"
    v log --format='%H %s' main..topic
    v status
    # exec writes into the work tree and runs from its root.
    rebase_i_run '2a exec echo made > made.txt'
    v status
    rebase_i_run '1a x pwd >/dev/null && test -f a'
    rebase_i_run '1a exec false'
    v rebase --abort
    rebase_i_run '1a exec'
    v rebase --abort
    v reflog
}

# `rebase --exec` without -i runs the interactive machinery on an unedited todo list: same progress
# output, a failing command stops with the interactive state, and a branch already on the target is
# still replayed so the command runs.
s_rebase_exec_plain() {
    rebase_repo
    g branch orig topic
    v rebase -x 'echo A' main topic
    echo "rc=$?"
    v log --format='%H %s' main..topic
    v reflog
    v switch -q topic
    v reset -q --hard orig
    v rebase -x 'echo A' -x 'echo B' main
    echo "rc=$?"
    v switch -q topic
    v reset -q --hard orig
    v rebase --exec 'false' main
    echo "rc=$?"
    v status
    v rebase --continue
    echo "rc=$?"
    v rebase --abort
    echo "rc=$?"
    v switch -q topic
    v reset -q --hard orig
    v rebase -x 'echo up' topic
    echo "rc=$?"
    v reflog
}

# `amend!` commits autosquash to `fixup -C`: the target's message is replaced by the amend commit's
# body (its `amend! …` subject line is dropped), fixups after it keep that message, a squash after
# it opens the editor on the combination with the `amend!` subject commented out.
s_rebase_autosquash_amend() {
    v init -q repo; cd repo
    echo base >base; v add base; v commit -q -m base
    v branch work
    tick; echo main >main.txt; v add main.txt; v commit -q -m "main moves"
    v switch -q work
    for n in A B C; do tick; echo $n >$n; v add $n; v commit -q -m "feat $n" -m "body $n"; done
    tick; echo 1 >f1; v add f1; v commit -q -m "amend! feat A" -m "new subject A" -m "new body A"
    tick; echo 2 >f2; v add f2; v commit -q -m "fixup! feat A"
    tick; echo 3 >f3; v add f3; v commit -q -m "amend! feat C"
    tick; echo 4 >f4; v add f4; v commit -q -m "squash! feat B"
    g branch orig
    rebase_i_helpers
    export GIT_EDITOR=true
    local mode
    for mode in "-i" ""; do
        v switch -q work; v reset -q --hard orig
        echo "== autosquash $mode"
        GIT_SEQUENCE_EDITOR=cat v rebase $mode --autosquash main
        echo "rc=$?"
        v log --format='%H%n%B' main..work
        v reflog
        v status
    done
    # The amend commit is the last link: squash after it keeps both messages.
    v switch -q work; v reset -q --hard orig
    tick; echo 5 >f5; v add f5; v commit -q -m "squash! feat A"
    echo "== amend then squash"
    GIT_SEQUENCE_EDITOR=cat v rebase -i --autosquash main
    echo "rc=$?"
    v log --format='%H%n%B' main..work
}

# `rebase --autosquash` without -i runs the interactive machinery on an unedited, reordered todo
# list; `rebase.autoSquash` alone does not (it only applies with -i), `--no-autosquash` cancels it.
s_rebase_autosquash_plain() {
    v init -q repo; cd repo
    echo base >base; v add base; v commit -q -m base
    v branch work
    tick; echo main >main.txt; v add main.txt; v commit -q -m "main moves"
    v switch -q work
    for n in A B C; do tick; echo $n >$n; v add $n; v commit -q -m "feat $n"; done
    tick; echo 1 >f1; v add f1; v commit -q -m "fixup! feat A"
    tick; echo 2 >f2; v add f2; v commit -q -m "squash! feat B"
    g branch orig
    export GIT_EDITOR=true
    local flags
    for flags in "--autosquash" "--autosquash --no-autosquash" "--no-autosquash --autosquash" "--no-autosquash"; do
        v switch -q work; v reset -q --hard orig
        echo "== $flags"
        v rebase $flags main
        echo "rc=$?"
        v log --format='%H %s' main..work
        v reflog
        v status
    done
    v switch -q work; v reset -q --hard orig
    v config rebase.autoSquash true
    echo "== config true, no -i"
    v rebase main
    echo "rc=$?"
    v log --format='%H %s' main..work
    v config --unset rebase.autoSquash
    # Already on the target: the branch is still replayed, as with --exec.
    v switch -q work; v reset -q --hard orig
    echo "== up to date"
    v rebase --autosquash work
    echo "rc=$?"
    v reflog
}

s_rebase_break() {
    rebase_repo
    g branch orig topic
    rebase_i_helpers
    # break stops between two commits; the todo keeps the rest and --continue goes on.
    rebase_i_run '2i break'
    v status
    v log --format='%H %s' main..topic
    v rebase --continue
    echo "rc=$?"
    v log --format='%H %s' main..topic
    v status
    # b is the short form, a break may stand last, and work done while stopped survives.
    v switch -q topic
    v reset -q --hard orig
    rebase_i_run '1a b'
    echo more >break-note.txt
    v add break-note.txt
    v commit -q -m 'added while stopped'
    v rebase --continue
    echo "rc=$?"
    v log --format='%s' main..topic
    v switch -q topic
    v reset -q --hard orig
    rebase_i_run '$a break'
    v rebase --continue
    echo "rc=$?"
    # arguments are an error
    rebase_i_run '1a break extra words'
    echo "rc=$?"
    v rebase --abort
    v reflog
}

# `fixup -C` / `fixup -c`: the message of that commit replaces the chain's message so far (no editor for
# -C, the editor for -c), unless a `squash` is already in the chain, when they act like `squash`. The
# intermediate commits of a chain are in the reflog, so their messages are compared by id as well.
s_rebase_fixup_c() {
    rebase_repo
    g branch orig topic
    rebase_i_helpers
    for edit in '2s/^pick/fixup -C/' '2s/^pick/fixup -c/' '3s/^pick/f -C/' '2s/^pick/fixup/;3s/^pick/fixup -C/' \
        '2s/^pick/fixup -C/;3s/^pick/fixup/' '2s/^pick/squash/;3s/^pick/fixup -C/' '2s/^pick/fixup -C/;3s/^pick/squash/' \
        '2s/^pick/fixup -C/;3s/^pick/fixup -C/' '2s/^pick/fixup -c/;3s/^pick/fixup/' '2s/^pick/fixup/;3s/^pick/fixup -c/' \
        '2s/^pick/fixup -c/;3s/^pick/fixup -C/' '1s/^pick/reword/;2s/^pick/fixup -C/'; do
        rebase_i_run "$edit"
        v reflog
    done
    # The todo list as saved: the flag is kept, the commit is named in full.
    rebase_i_run '2s/^pick/fixup -C/;3a exec cat .git/rebase-merge/done'
    # Invalid lines: git parses the word after the flag as the commit.
    rebase_i_run '2s/^pick/fixup -x/'
    v rebase --abort
    rebase_i_run '2s/^pick \([0-9a-f]*\).*/fixup -C/'
    v rebase --abort
    rebase_i_run '2s/^pick/fixup -C -c/'
    v rebase --abort
    rebase_i_run '2s/^pick/squash -C/'
    v rebase --abort
    # The flag glued to the commit parses like the spaced form.
    rebase_i_run '2s/^pick \([0-9a-f]*\).*/fixup -C\1/'
    v reflog
    rebase_i_run '2s/^pick \([0-9a-f]*\).*/fixup -c\1/'
    v reflog
    rebase_i_run '2s/^pick/fixup -Cxyz/'
    v rebase --abort
    v reflog
}

# `rebase --keep-base`: onto the merge base of upstream and branch, so a linear branch is "up to
# date" and `-i` replays it in place; `--onto`/`--root` conflict; unrelated histories are refused.
s_rebase_keep_base() {
    rebase_repo
    g branch orig topic
    rebase_i_helpers
    v switch -q topic
    v rebase --keep-base main
    echo "rc=$?"
    v rebase --keep-base main topic
    echo "rc=$?"
    v switch -q main
    v rebase --keep-base main topic
    echo "rc=$?"
    v log --oneline --all
    v rebase --keep-base --onto main main
    echo "rc=$?"
    v rebase --keep-base --root
    echo "rc=$?"
    v rebase --keep-base
    echo "rc=$?"
    v rebase --keep-base nosuch
    echo "rc=$?"
    # -i keeps the base: reword and drop land on top of A, main's B and C stay out.
    v switch -q topic
    SEQ='1s/^pick/reword/;2d' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" GIT_EDITOR="$PWD/../msg.sh" v rebase -i --keep-base main
    echo "rc=$?"
    v log --format='%H %an %s' --graph --all
    v reflog
    cat .git/ORIG_HEAD
    # A merge on the branch is flattened by the plain form; the commits already on the base are
    # stepped over.
    v reset -q --hard orig
    v switch -q -c side orig~3
    echo s >s; v add s; tick; v commit -q -m S
    v switch -q topic
    tick; v merge -q --no-edit side >/dev/null 2>&1
    v log --oneline --graph topic
    v rebase --keep-base main
    echo "rc=$?"
    v log --oneline --graph topic
    v reflog
    # No merge base at all.
    v switch -q --orphan unrelated
    echo u >u; v add u; tick; v commit -q -m U
    v switch -q topic
    v rebase --keep-base unrelated
    echo "rc=$?"
    v status
}

# Too many positionals: git prints its whole usage text on stderr and exits 129 whether or not
# `--root` is given, before it touches anything (no pre-rebase hook, no state, no reflog entry).
s_rebase_positional_usage() {
    rebase_repo
    mkdir -p .git/hooks
    printf '#!/bin/sh\necho "pre-rebase $@" >&2\n' >.git/hooks/pre-rebase
    chmod +x .git/hooks/pre-rebase
    v rebase --root main topic
    echo "rc=$?"
    v rebase --root main topic extra
    echo "rc=$?"
    v rebase main topic extra
    echo "rc=$?"
    v rebase -i main topic extra more
    echo "rc=$?"
    v rebase --onto main main topic extra
    echo "rc=$?"
    v status --short --branch
    v reflog -3
    ls .git | grep -c rebase
}

# `rebase --root`: without `--onto` the commits are replayed onto a virtual empty commit (and
# reused as fast-forwards); with `--onto` the range is everything not reachable from the new base,
# so a root commit already below it is stepped over; an unrelated root conflicts like any pick.
s_rebase_root() {
    rebase_repo
    v switch -q topic
    v rebase --root
    echo "rc=$?"
    v log --format='%H %p %s' --graph
    v reflog
    cat .git/ORIG_HEAD
    ls .git/rebase-merge
    v fsck
    v switch -q main
    v rebase --root -q
    echo "rc=$?"
    v rebase --root topic
    echo "rc=$?"
    v log --format='%H %p %s' --graph --all
    v reflog
    v reset -q --hard topic
    # The root is reachable from the new base: only the commits above it move.
    v switch -q -c moved main
    v rebase --root --onto topic
    echo "rc=$?"
    v log --format='%H %p %s' --graph
    v reflog -8
    # Onto itself and onto a commit inside the branch: nothing to replay.
    v rebase --root --onto moved
    echo "rc=$?"
    v rebase --root --onto moved~1
    echo "rc=$?"
    v reflog -4
    # An unrelated root is replayed like any pick and can conflict.
    v switch -q --orphan lone
    printf 'x\n' >a; echo l >l; v add a l; tick; v commit -q -m L1
    echo l2 >>l; tick; v commit -q -a -m L2
    v rebase --root --onto main
    echo "rc=$?"
    v status
    cat .git/rebase-merge/done
    printf 'x\n' >a; v add a
    v rebase --continue >/dev/null 2>&1
    echo "rc=$?"
    v log --format='%H %p %s' --graph
    v reflog -8
    # pre-rebase gets `--root` where the upstream would be, and the branch if one was named.
    mkdir -p .git/hooks
    printf '#!/bin/sh\necho "pre-rebase $@" >&2\n' >.git/hooks/pre-rebase
    chmod +x .git/hooks/pre-rebase
    v rebase --root
    echo "rc=$?"
    v rebase --root --onto main~1 main
    echo "rc=$?"
    printf '#!/bin/sh\nexit 1\n' >.git/hooks/pre-rebase
    v rebase --root
    echo "rc=$?"
}

# `rebase -i --root`: the todo list starts at the root commit and the footer names the virtual empty
# base; unedited picks fast-forward onto it, and reword/edit/squash/drop/exec work on the root like on
# any other commit, with and without `--onto`.
s_rebase_root_i() {
    rebase_repo
    rebase_i_helpers
    v switch -q topic
    g branch orig topic
    GIT_SEQUENCE_EDITOR=cat v rebase -i --root
    echo "rc=$?"
    v log --format='%H %p %s' --graph
    v reflog
    cat .git/ORIG_HEAD
    ls .git/rebase-merge
    v fsck
    # Reword the root itself, drop a middle commit.
    v reset -q --hard orig
    SEQ='1s/^pick/reword/;2d' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" GIT_EDITOR="$PWD/../msg.sh" v rebase -i --root
    echo "rc=$?"
    v log --format='%H %p %an %s%n%b' --graph
    v reflog -8
    # Stop on the root, amend it, continue; then squash the second commit into it.
    v reset -q --hard orig
    SEQ='1s/^pick/edit/' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase -i --root
    echo "rc=$?"
    v status
    cat .git/rebase-merge/done
    echo amended >>a; v commit -q -a --amend -m 'root amended'
    v rebase --continue
    echo "rc=$?"
    v log --format='%H %p %s' --graph
    v reset -q --hard orig
    SEQ='2s/^pick/squash/' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" GIT_EDITOR=true v rebase -i --root
    echo "rc=$?"
    v log --format='%H %p %s%n%b' --graph
    v reflog -8
    # Dropping the root leaves the next commit as the new root.
    v reset -q --hard orig
    SEQ='1d' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase -i --root
    echo "rc=$?"
    v log --format='%H %p %s' --graph
    v status
    cat .git/rebase-merge/squash-onto
    v rebase --abort
    # `--exec`, a branch argument, and an interactive `--root --onto`.
    v reset -q --hard orig
    v switch -q main
    GIT_SEQUENCE_EDITOR=cat v rebase -i --root -x 'echo X' topic
    echo "rc=$?"
    v log --format='%H %p %s' --graph
    v reset -q --hard orig
    v switch -q -c moved main
    GIT_SEQUENCE_EDITOR=cat v rebase -i --root --onto topic
    echo "rc=$?"
    v log --format='%H %p %s' --graph
    v reflog -8
    # An unrelated root conflicts like any pick; the todo and `done` show the root.
    v switch -q --orphan lone
    printf 'x\n' >a; echo l >l; v add a l; tick; v commit -q -m L1
    echo l2 >>l; tick; v commit -q -a -m L2
    GIT_SEQUENCE_EDITOR=cat v rebase -i --root --onto main
    echo "rc=$?"
    v status
    cat .git/rebase-merge/done
    printf 'x\n' >a; v add a
    v rebase --continue >/dev/null 2>&1
    echo "rc=$?"
    v log --format='%H %p %s' --graph
    v reflog -8
    # pre-rebase sees `--root` where the upstream would be.
    mkdir -p .git/hooks
    printf '#!/bin/sh\necho "pre-rebase $@" >&2\n' >.git/hooks/pre-rebase
    chmod +x .git/hooks/pre-rebase
    GIT_SEQUENCE_EDITOR=cat v rebase -i --root
    echo "rc=$?"
}

# `stash branch`: the argument errors, a branch made at the stash's base with the index and the
# untracked files restored, `post-checkout` for the new branch, an explicit stash, and the two ways
# it can fail (the checkout refuses, the apply refuses) which must keep the stash.
s_stash_branch() {
    v init -q repo; cd repo
    echo a >a; echo b >b; v add .; v commit -q -m one
    mkdir -p .git/hooks
    printf '#!/bin/sh\necho "post-checkout $@" >&2\n' >.git/hooks/post-checkout
    chmod +x .git/hooks/post-checkout
    echo a2 >>a; echo new >n; v add n; echo ut >u
    v stash -q -u
    tick; echo b2 >>b; v commit -q -a -m two
    v stash branch
    v stash branch x stash@{3}
    v stash branch main
    v stash branch x stash@{0} y
    v stash list
    v stash branch nb
    echo "rc=$?"
    v status
    v stash list
    v reflog -5
    v log --oneline -3 --all
    v switch -q main
    # An explicit stash, not the newest one.
    tick; echo c >>a; v commit -q -a -m three
    echo a3 >>a; v stash -q
    echo c1 >c; v add c; v stash -q
    v stash list
    v stash branch cb stash@{1}
    echo "rc=$?"
    v status
    v stash list
    v branch
    # The checkout refuses: a local change to a file that differs between here and the stash base.
    v switch -q main
    echo ok >>b; v stash -q
    tick; echo d >>b; v commit -q -a -m four
    echo dirty >>b
    v stash branch nope
    echo "rc=$?"
    v status
    v stash list
    v branch
    v checkout -q -- b
    # The apply refuses: the checkout carries a local change that the stash also touches.
    v switch -q main
    echo z >>a; v stash -q
    echo "z-local" >>a
    v stash branch carry
    echo "rc=$?"
    v status
    v stash list
    v branch
    # `pop --index` that conflicts says the index was not unstashed.
    v reset -q --hard; rm -f u; v switch -q main
    echo s >>b; v stash -q
    echo h >>b; tick; v commit -q -a -m five
    v stash pop --index
    echo "rc=$?"
    v stash list
}

# `stash save` parses its words like git's parse_options (options anywhere, a lone `--` ends them and is kept
# in the message, unknown options print the usage and exit 129); `stash store` of a non-commit adds the
# `error: object <id> is a tree, not a commit` line.
s_stash_errors() {
    v init -q repo; cd repo
    echo a >a; v add a; v commit -q -m one
    v stash save --bogus
    echo "rc=$?"
    v stash save -z
    echo "rc=$?"
    v stash save -m
    echo "rc=$?"
    v stash save --message
    echo "rc=$?"
    v stash save -h
    echo "rc=$?"
    local i=0 args
    for args in '-- foo bar' 'foo -k bar' '-m msg word' '-m msg' '--message=foo' '-mfoo' '--keep -q x' \
        '--no-keep-index x' '-- -k' 'x -- y' '-q' '-ku z'; do
        i=$((i + 1))
        echo "mod$i" >>a
        v stash save $args
        echo "rc=$?"
        v stash list
        v status --short
        v reset -q --hard
    done
    echo mod >>a
    v stash save -u -- x y
    echo "rc=$?"
    v stash list
    v stash clear
    v stash store 'HEAD^{tree}'
    echo "rc=$?"
    v stash store HEAD
    echo "rc=$?"
    v stash store nonexistent
    echo "rc=$?"
    v stash list
}

# `grep --threads=<n>` only changes how many workers git uses, so the output is the same; a negative count
# dies with git's `invalid number of threads`, a non-integer fails option parsing (129), `--no-threads` resets.
s_grep_threads() {
    v init -q repo; cd repo
    printf 'foo\nbar\n' >a.txt
    v add .; v commit -q -m one
    local args
    for args in '--threads=2' '--threads=0' '--threads=1k' '--no-threads' '--threads=3 --no-threads' \
        '--threads=-1' '--threads=x' '--threads=' '--threads=2 -c' '--threads=-2 --no-threads'; do
        v grep $args foo
        echo "rc=$?"
    done
    v grep --threads 2 foo
    echo "rc=$?"
    v grep --threads -3 foo
    echo "rc=$?"
    v grep --threads
    echo "rc=$?"
    v grep --threads=-1
    echo "rc=$?"
}

# `grep -o` prints each match on a line of its own (leftmost across patterns, the longest on a tie, an
# empty match ends the line, `-v` switches it off, context lines are silent but still draw `--`);
# `--heading` names the file once above its lines, `-h` does not hide it, the last of
# `--heading` / `--no-heading` wins.
s_grep_output_modes() {
    v init -q repo; cd repo
    printf 'foo bar foo\nnone\nxfoo\n' >a.txt
    printf 'foo\nbaz foo foo\n' >b.txt
    printf 'q\nfoo\nq\nq\nq\nfoo\n' >d.txt
    printf 'a\0foo\n' >bin.dat
    printf 'Foo FOO foobar\nfoo-bar_foo\n' >e.txt
    v add .; v commit -q -m one
    v grep -o foo
    v grep -on foo
    v grep -oh foo
    v grep -o -w foo
    v grep -o -i foo e.txt
    v grep -o -c foo
    v grep -o -v foo a.txt
    v grep -o -e foo -e bar a.txt
    v grep -o -e 'fo*' -e 'foo bar' a.txt
    v grep -oE 'o*' a.txt
    v grep -oE '^foo|foo$' a.txt
    v grep -o '^foo' a.txt
    v grep -o -l foo
    v grep -o -a foo bin.dat
    v grep -o --cached foo b.txt
    v grep -o foo HEAD -- b.txt
    v grep -o --no-only-matching foo a.txt
    v grep -o -A1 foo a.txt d.txt
    v grep -o -B1 foo d.txt
    v grep -o -C1 -n foo d.txt a.txt
    v grep -o -v -A1 foo d.txt
    v grep --heading foo
    v grep --heading -n foo
    v grep --heading -h foo a.txt
    v grep --heading -A1 foo a.txt d.txt
    v grep --heading -B1 foo d.txt
    v grep --heading -o -B1 foo d.txt a.txt
    v grep --heading -c foo
    v grep --heading -l foo
    v grep --heading --no-heading foo a.txt
    v grep --no-heading --heading foo a.txt
    v grep --heading foo HEAD -- b.txt
    v grep --heading nonexistent
    echo "rc=$?"
}

# `grep -z` turns every separator after a name or line number (and the newline after `-l`/`-L` names)
# into NUL; `--break` puts an empty line before each file's output after the first, in place of the
# `--` that context prints between files. A binary match under `--break` is refused by name (git's
# output for it depends on its worker threads; Rust test `grep_break_refuses_binary`).
s_grep_null_break() {
    v init -q repo; cd repo
    printf 'foo\nq\nfoo\n' >a.txt
    printf 'x\nfoo\n' >b.txt
    printf 'q\nfoo\nq\nq\nq\nfoo\n' >d.txt
    printf 'a\0foo\n' >bin.dat
    v add .; v commit -q -m one
    v grep -z foo
    v grep -z -n foo
    v grep -z -c foo
    v grep -z -h -c foo
    v grep -z -l foo
    v grep -z -L foo b.txt a.txt
    v grep -z -L nomatch b.txt
    v grep -z -A1 foo d.txt
    v grep -z -n -C1 foo d.txt
    v grep -z --heading -n foo a.txt
    v grep -z foo bin.dat
    v grep -z --no-null -l foo a.txt
    v grep --break foo a.txt b.txt d.txt
    v grep --break -n -A1 foo a.txt d.txt b.txt
    v grep --break -C1 foo a.txt d.txt
    v grep --break --heading -A1 foo a.txt d.txt
    v grep --break -a foo bin.dat d.txt
    v grep --break -I foo bin.dat d.txt
    v grep --break -c foo bin.dat d.txt
    v grep --break -c foo
    v grep --break -l foo
    v grep --break -o -A1 foo d.txt a.txt
    v grep --break -o -B1 foo d.txt a.txt
    v grep --break --no-break foo a.txt b.txt bin.dat d.txt
    v grep --no-break --break foo a.txt b.txt d.txt
    v grep -z --break -n foo a.txt b.txt d.txt
    v grep --break foo HEAD -- a.txt b.txt
    v grep --break nonexistent
    echo "rc=$?"
}

# `grep --untracked`: untracked files that are not ignored are searched too, merged with the tracked
# ones in path order; it cannot be combined with `--cached`; a revision is read as a path.
s_grep_untracked() {
    v init -q repo; cd repo
    printf 'needle\n' >b.txt
    mkdir d
    printf 'needle\n' >d/z.txt
    printf 'needle\n' >d/a.txt
    v add .; v commit -q -m one
    printf 'needle\n' >a_new.txt
    printf 'needle\n' >d/new2.txt
    mkdir e
    printf 'needle\n' >e/q.txt
    printf 'ign*\n' >.gitignore
    printf 'needle\n' >ignored.txt
    printf 'needle\n' >zz
    v grep needle
    v grep --untracked needle
    v grep --untracked -l needle
    v grep --untracked -c needle
    v grep --untracked -n needle d
    v grep --untracked nomatch
    echo "rc=$?"
    v grep --cached --untracked needle
    echo "rc=$?"
    v grep --untracked needle HEAD
    echo "rc=$?"
    cd d
    v grep --untracked needle
}

# `label` / `reset` todo lines: a label is a ref under refs/rewritten that lives until the rebase ends
# (finished or aborted), `reset` moves HEAD and the work tree there (reflog `rebase (reset): '<arg>'`, none
# when HEAD does not move), a name that does not resolve reschedules the line. `merge` and `update-ref`
# stay refused by name (GAPS.md).
s_rebase_label_reset() {
    rebase_repo
    g branch orig topic
    rebase_i_helpers
    # Rebuild the same commits after a reset to main: a fork off the label, then back to the label.
    rebase_i_run $'1i label start\n1i reset main\n3a reset start\n3a label end'
    v for-each-ref
    v reflog
    # Stopped between lines: the labels exist, the done file does not matter, --continue cleans them.
    rebase_i_run $'1i label a\n2i label b\n2i break\n3a reset a'
    v for-each-ref
    v rev-parse refs/rewritten/a refs/rewritten/b
    v rebase --continue
    echo "rc=$?"
    v for-each-ref
    v reflog
    # The short forms, a branch name and a commit id as reset targets.
    rebase_i_run $'1i l first\n1a t main\n2a t first\n3a t orig'
    v reflog
    rebase_i_run $'2i break\n2i t main'
    v rebase --continue
    echo "rc=$?"
    v log --format='%H %s' main..topic
    # A name that resolves to nothing stops with git's hint; the line stays in the todo list.
    rebase_i_run $'1i reset nosuch'
    v status
    v rebase --abort
    v for-each-ref
    v reflog
    # Labels are checked when the list is read.
    for bad in 'label' 'label bad name' 'label ..x' 'label a.lock' 'label @' 'reset' 'label a # c'; do
        rebase_i_run "1i $bad"
        v rebase --abort
    done
    # A label aborted over disappears with the rebase.
    rebase_i_run $'1i label gone\n2i break'
    v rebase --abort
    v for-each-ref
}

# `update-ref <ref>` todo lines: each names a branch that is moved to the rewritten HEAD when the rebase
# finishes (never earlier, never on abort); the list is checked when it is read.
s_rebase_update_ref() {
    rebase_repo
    g branch orig topic
    g branch bD orig~2
    g branch bE orig~1
    rebase_i_helpers
    rebase_i_run $'1a update-ref refs/heads/bD\n2a update-ref refs/heads/bE'
    v for-each-ref
    v reflog bD
    v reflog bE
    v reflog
    # Stopped before the end: nothing moved yet, the state file is git's, --continue applies them.
    g branch -f bD orig~2; g branch -f bE orig~1
    rebase_i_run $'1a update-ref refs/heads/bD\n2a break\n2a update-ref refs/heads/bE'
    v for-each-ref
    cat .git/rebase-merge/update-refs
    v rebase --continue
    echo "rc=$?"
    v for-each-ref
    # The short form, a repeated line, a ref that does not exist yet, a ref outside refs/heads.
    g branch -f bD orig~2; g branch -f bE orig~1
    rebase_i_run $'1a u refs/heads/bD\n1a update-ref refs/heads/bD\n2a update-ref refs/heads/new\n3a update-ref refs/tags/tg'
    v for-each-ref
    # Abort leaves every ref where it was.
    g branch -f bD orig~2
    rebase_i_run $'1a update-ref refs/heads/bD\n2a break'
    v rebase --abort
    v for-each-ref
    # The line is checked when the list is read.
    for bad in 'update-ref' 'update-ref bad name' 'update-ref refs/heads/a..b' 'update-ref HEAD' 'update-ref bD'; do
        rebase_i_run "1a $bad"
        v rebase --abort
    done
}

# `--update-refs` / `rebase.updateRefs`: git writes an `update-ref` line after every commit a local branch points
# at (reverse name order within one commit, a branch checked out elsewhere as a comment, the rebased branch,
# tags and the base ignored), before `--autosquash` reorders, and moves the branches when the rebase ends.
s_rebase_update_refs() {
    rebase_repo
    g tag orig topic
    rebase_i_helpers
    ur_reset() {
        [ -d .git/rebase-merge ] && v rebase --abort
        v switch -q topic
        v reset -q --hard orig
        for b in bD bE aE bG bBase; do g branch -D "$b" >/dev/null 2>&1; done
        g branch bD orig~2; g branch bE orig~1; g branch aE orig~1; g branch bG orig; g branch bBase main
        g tag -f tg orig~1 >/dev/null
    }
    ur_reset
    # Already on top of the base: git says so before it looks at the branches.
    v rebase --update-refs topic
    echo "rc=$?"
    v for-each-ref
    # The todo list the editor sees, the refs afterwards, the reflogs.
    GIT_SEQUENCE_EDITOR=cat v rebase -i --update-refs main
    echo "rc=$?"
    v for-each-ref
    v reflog bE
    v reflog
    # No -i: the flag alone runs the todo list unedited.
    ur_reset
    v rebase --update-refs main
    echo "rc=$?"
    v for-each-ref
    # The config key, and --no-update-refs beating it (and --update-refs beating a false key).
    ur_reset
    g config rebase.updateRefs true
    v rebase main
    v for-each-ref
    ur_reset
    v rebase --no-update-refs main
    v for-each-ref
    ur_reset
    g config rebase.updateRefs false
    v rebase --update-refs main
    v for-each-ref
    g config --unset rebase.updateRefs
    # Stopped half way: nothing moved yet, the state file lists the refs by name, --continue applies them.
    ur_reset
    SEQ=$'2a break\n5a break' GIT_SEQUENCE_EDITOR="$PWD/../seq.sh" v rebase -i --update-refs main
    echo "rc=$?"
    v for-each-ref
    cat .git/rebase-merge/update-refs
    # The state files: git keeps the blank line after each generated update-ref, and the backup is the
    # list as the editor was given it, footer included.
    for f in git-rebase-todo done git-rebase-todo.backup end msgnum; do
        echo "-- $f" >>"$TRANSCRIPT"
        cat -A .git/rebase-merge/$f >>"$TRANSCRIPT"
    done
    v status
    v rebase --continue
    echo "rc=$?"
    for f in git-rebase-todo done; do
        echo "-- $f" >>"$TRANSCRIPT"
        cat -A .git/rebase-merge/$f >>"$TRANSCRIPT"
    done
    v status
    v rebase --continue
    echo "rc=$?"
    v for-each-ref
    # A fixup commit with a branch on it: the update-ref line stays where the fixup was, --autosquash
    # moves only the commit line, and --exec lands before the update-ref lines.
    ur_reset
    tick; echo x >>d; v commit -q -a -m 'fixup! D'
    g branch onFix
    tick; echo y >g; v commit -q -a -m H
    g tag -f orig topic
    GIT_SEQUENCE_EDITOR=cat v rebase -i --update-refs --autosquash --exec true main
    echo "rc=$?"
    v for-each-ref
    v log --oneline --all
    # A branch checked out in another work tree is only a comment, and is left alone.
    ur_reset
    g tag -f orig topic
    g branch wtB orig~1
    v worktree add -q ../wtB wtB
    GIT_SEQUENCE_EDITOR=cat v rebase -i --update-refs main
    echo "rc=$?"
    v for-each-ref
    # Behind the base: git fast-forwards, whatever the flag.
    v worktree remove --force ../wtB
    g branch -D wtB >/dev/null
    ur_reset
    v switch -q -c behind main~1
    v rebase --update-refs main
    echo "rc=$?"
    v for-each-ref
    v reflog
    v switch -q topic
    g branch -D behind >/dev/null
    # Detached HEAD: no branch is excluded.
    ur_reset
    v switch -q --detach topic
    GIT_SEQUENCE_EDITOR=cat v rebase -i --update-refs main
    echo "rc=$?"
    v for-each-ref
}

s_sparse_clean() {
    v init -q repo; cd repo
    mkdir -p a b/deep b/other c/y c/z d/e; echo 1 >a/f; echo 2 >b/f; echo 3 >b/deep/g; echo 4 >b/other/f
    echo 5 >c/f; echo 6 >c/y/g; echo 7 >c/z/h; echo 8 >d/e/f; echo 9 >top
    v add .; v commit -q -m one
    # Not sparse yet, then free-form patterns: both refuse before looking at the options.
    v sparse-checkout clean -n; echo "rc=$?"
    v sparse-checkout clean --bogus; echo "rc=$?"
    v sparse-checkout set --no-cone /a/; echo "rc=$?"
    v sparse-checkout clean -n; echo "rc=$?"
    v sparse-checkout clean --bogus; echo "rc=$?"
    v sparse-checkout set --cone a b/deep
    # Nothing left over: silent, with or without -f.
    v sparse-checkout clean -n; echo "rc=$?"
    v sparse-checkout clean -f; echo "rc=$?"
    v sparse-checkout clean; echo "rc=$?"
    v sparse-checkout clean --no-force --no-dry-run; echo "rc=$?"
    v sparse-checkout clean --bogus; echo "rc=$?"
    v sparse-checkout clean -x; echo "rc=$?"
    # Leftovers: untracked and ignored files under skipped directories, a modified tracked file that
    # keeps c/ alive, untracked directories git never looked at, files inside the cone.
    echo '*.log' >.git/info/exclude
    mkdir -p b/other/sub d/e/new zz a/inner
    echo u >b/other/sub/u; echo i >b/other/x.log; echo n >d/e/new/n; echo t >d/top.txt
    echo q >zz/q; echo m >a/inner/m; echo u >b/uf
    v sparse-checkout clean -n; echo "rc=$?"
    vs sparse-checkout clean -nv; echo "rc=$?"
    v sparse-checkout clean -n -f; echo "rc=$?"
    v sparse-checkout clean -f; echo "rc=$?"
    mkdir -p d/e/new; echo n >d/e/new/n
    v sparse-checkout set a c; echo mod >c/f
    v sparse-checkout set a
    mkdir -p c/y c/z; echo u >c/y/u; echo u >c/z/u
    vs sparse-checkout clean -nv; echo "rc=$?"
    vs sparse-checkout clean -fv; echo "rc=$?"
    v sparse-checkout clean -f -n; echo "rc=$?"
    v sparse-checkout clean -f -- extra; echo "rc=$?"
    mkdir -p d/e/new; echo n >d/e/new/n
    (cd d/e/new && vs sparse-checkout clean -f -v; echo "rc=$?")
    (cd a && v sparse-checkout clean -n; echo "rc=$?")
}

s_sparse_leftovers() {
    v init -q repo; cd repo
    mkdir -p d/e f; echo 1 >a; echo 2 >d/x; echo 3 >d/e/y; echo 4 >f/z
    v add .; v commit -q -m one
    v switch -q -c side; echo s >f/z; echo sx >d/x; v commit -q -a -m side
    v switch -q main; echo m >f/z; tick; v commit -q -a -m main
    v sparse-checkout set d f
    # check-rules: the current definition, quoting in and out, -z, odd names.
    vin 'a
d/x
d/e/y
f/z
zz/q
d
' sparse-checkout check-rules
    echo "rc=$?"
    vin '"a"
d/\303\251
"d/\303\251"
d/a b
"d/tab\t"
./a
f//z
d/
../x
' sparse-checkout check-rules
    STDIN_FILE=$(mktemp "${TMPDIR:-/tmp}/stdin.XXXXXX"); printf 'a\0d/e/y\0zz/q\0"a"\0f/z' >"$STDIN_FILE"
    # The NUL-separated output goes through od so the transcript stays text.
    echo '$ check-rules -z | od -c' >>"$TRANSCRIPT"
    if [ "$IMPL" = git ]; then "$GIT" sparse-checkout check-rules -z <"$STDIN_FILE" | od -c >>"$TRANSCRIPT"; else "$RIVET" sparse-checkout check-rules -z <"$STDIN_FILE" | od -c >>"$TRANSCRIPT"; fi
    rm -f "$STDIN_FILE"; STDIN_FILE=
    vin '"abc
' sparse-checkout check-rules
    vin 'a
' sparse-checkout check-rules extra args
    vin 'a
' sparse-checkout check-rules --bogus
    printf '/*\n!/*/\n/d/\n!/d/*/\n/d/e/\n' >"$PWD/../cone.rules"
    printf '/a\n/d/\n!/d/x\n' >"$PWD/../free.rules"
    : >"$PWD/../empty.rules"
    for rules in cone free empty; do
        for flag in "" --cone --no-cone; do
            vin 'a
d/x
d/e/y
f/z
d
d/e
' sparse-checkout check-rules $flag --rules-file "$PWD/../$rules.rules"
        done
    done
    vin 'a
' sparse-checkout check-rules --rules-file="$PWD/../cone.rules"
    vin 'a
' sparse-checkout check-rules --rules-file "$PWD/../missing.rules"
    echo "rc=$?"
    # Free-form definition, and a repository where the pattern file is gone.
    v sparse-checkout set --no-cone '/a' '/d/' '!/d/e/'
    vin 'a
d/x
d/e/y
f/z
' sparse-checkout check-rules
    rm .git/info/sparse-checkout
    vin 'a
' sparse-checkout check-rules
    echo "rc=$?"
    v sparse-checkout set --cone d f
    # ls-files --sparse lists every index entry when the index is not sparse.
    v ls-files --sparse
    v ls-files -t --sparse
    # status says how much of the tree is present.
    v status
    echo more >>d/x; echo staged >>a; v add a
    v status
    v reset -q a; v checkout -q -- a d/x
    # set / add from a subdirectory: arguments relative to it, --stdin relative to the root.
    (cd d; v sparse-checkout set e); v sparse-checkout list; ls
    (cd d; v sparse-checkout add ../f); v sparse-checkout list
    (cd d; vin 'f
' sparse-checkout set --stdin); v sparse-checkout list
    v sparse-checkout set --cone d f
    (cd d; v sparse-checkout set ..); v sparse-checkout list
    v sparse-checkout set --cone d
    (cd d; v sparse-checkout set ../../x; echo "rc=$?")
    (cd d; v sparse-checkout set /f; echo "rc=$?")
    (cd d; v sparse-checkout add x; echo "rc=$?")
    v sparse-checkout list
    (cd d; v sparse-checkout set --no-cone '/a'; echo "rc=$?")
    (cd d; v sparse-checkout add '/a'; echo "rc=$?")
    v sparse-checkout list
    v sparse-checkout set --cone d
    cat .git/info/sparse-checkout
    # A merge whose conflict lies outside the definition: the file is written, then stays.
    v merge side
    echo "rc=$?"
    v status
    v ls-files -t
    v ls-files -u
    cat f/z
    v merge --abort
    echo "rc=$?"
    ls; v ls-files -t
    v status --short
}

# Interop seam: a rebase that one implementation starts and stops, the other one resumes. In the
# "git" run both are git; in the "rivet" run the first half is rivet (`v`) and the second real git
# (`vg`), and then the other way round, so the transcripts, ids, reflogs and files must be identical.
s_rebase_interop() {
    v init -q repo; cd repo
    echo 0 >f; v add f; v commit -q -m base
    v switch -q -c topic
    for i in 1 2 3 4; do echo $i >f$i; v add f$i; tick; v commit -q -m c$i; done
    v switch -q main
    echo m >m; v add m; tick; v commit -q -m main
    echo conflict >f3; v add f3; tick; v commit -q -m "main f3"
    orig=$(g rev-parse topic)
    seq="sed -i -e 's/^pick \(.*\) # c1\$/edit \1 # c1/' -e 's/^pick \(.*\) # c2\$/squash \1 # c2/' -e 's/^pick \(.*\) # c4\$/reword \1 # c4/'"
    # rivet starts, git resumes: stop at the edit, a squash behind it, a conflict, a reword.
    v switch -q topic
    GIT_SEQUENCE_EDITOR="$seq" v rebase -i main
    echo "rc=$?"
    echo more >>f1; v commit -q -a --amend --no-edit
    vg rebase --continue
    echo "rc=$?"
    vg status
    echo res >f3; v add f3
    vg rebase --continue
    echo "rc=$?"
    v status
    v log --format='%h %s' main..topic
    v reflog -14
    # rivet starts, git skips the conflicting commit, then aborts a second run.
    v switch -q -C topic "$orig"
    GIT_SEQUENCE_EDITOR="$seq" v rebase -i main
    vg rebase --continue
    vg rebase --skip
    echo "rc=$?"
    v log --format='%h %s' main..topic
    v switch -q -C topic "$orig"
    GIT_SEQUENCE_EDITOR="$seq" v rebase -i main
    vg rebase --abort
    echo "rc=$?"
    v status
    # git starts, rivet resumes.
    v switch -q -C topic "$orig"
    GIT_SEQUENCE_EDITOR="$seq" vg rebase -i main
    echo "rc=$?"
    echo more >>f1; v commit -q -a --amend --no-edit
    v rebase --continue
    echo "rc=$?"
    v status
    echo res >f3; v add f3
    v rebase --continue
    echo "rc=$?"
    v status
    v log --format='%h %s' main..topic
    v reflog -14
    v switch -q -C topic "$orig"
    GIT_SEQUENCE_EDITOR="$seq" vg rebase -i main
    v rebase --continue
    v rebase --skip
    echo "rc=$?"
    v log --format='%h %s' main..topic
    v switch -q -C topic "$orig"
    GIT_SEQUENCE_EDITOR="$seq" vg rebase -i main
    v rebase --abort
    echo "rc=$?"
    v status
}

# `--no-edit`: `commit --amend` keeps the message (cleaned up as Git's whitespace mode does) and
# opens no editor; `merge` and `cherry-pick` accept the flag.
s_commit_no_edit() {
    v init -q repo; cd repo
    v commit --amend --no-edit
    echo "rc=$?"
    echo 0 >f; v add f; tick
    v commit -q -m "subj

body  
 
 
trail"
    echo 1 >>f; v add f; tick
    v commit --amend --no-edit
    echo "rc=$?"
    v log -1 --format=%B | od -c
    v cat-file -p HEAD | tail -n +5 | od -c
    echo 2 >>f; v add f; tick
    v commit --amend --no-edit -m "explicit"
    v log --format=%s
    v switch -q -c topic
    echo t >t; v add t; tick; v commit -q -m "on topic"
    v switch -q main
    echo m >m; v add m; tick; v commit -q -m "on main"
    v cherry-pick --no-edit topic
    echo "rc=$?"
    v log --format=%s
    v reset -q --hard HEAD~1
    v merge --no-edit topic
    echo "rc=$?"
    v log --format=%s
}

# `submodule update --remote`: fetch the submodule's own remote and check out `submodule.<name>.branch`
# of `.gitmodules` (the remote HEAD when unset) instead of the commit the superproject records.
s_submodule_update_remote() {
    sm_nested_top
    g clone -q top clone
    cd clone
    v submodule update --init vendor/lib
    (cd "$ROOT/lib" && tick && echo two >>l.txt && g commit -q -a -m two && g branch dev &&
        g checkout -q dev && tick && echo d >d.txt && g add d.txt && g commit -q -m dev && g checkout -q main)
    v submodule update --remote vendor/lib
    sm_snap vendor/lib
    v submodule status vendor/lib
    v submodule update --remote vendor/lib
    g config -f .gitmodules submodule.vendor/lib.branch dev
    v submodule update --remote vendor/lib
    sm_snap vendor/lib
    (cd "$ROOT/lib" && tick && echo three >>l.txt && g checkout -q dev && echo e >>d.txt && g commit -q -a -m dev2 && g checkout -q main)
    v submodule update -N --remote vendor/lib
    v submodule update -q --remote vendor/lib
    sm_snap vendor/lib
    v submodule update --remote --checkout vendor/lib
    g config -f .gitmodules submodule.vendor/lib.branch nosuch
    v submodule update --remote vendor/lib
    echo "rc=$?"
    g config -f .gitmodules --unset submodule.vendor/lib.branch
    rm -rf vendor/lib
    v submodule update --remote vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    v submodule update --remote --init
    echo "rc=$?"
    sm_snap other
}

# `submodule update --merge`: run `git merge <recorded commit>` inside the submodule instead of detaching.
s_submodule_update_merge() {
    sm_nested_top
    g clone -q top clone
    cd clone
    v submodule update --init vendor/lib
    g -C vendor/lib checkout -q -b work
    (cd "$ROOT/lib" && tick && echo two >>l.txt && g commit -q -a -m two)
    v submodule update --merge --remote vendor/lib
    sm_snap vendor/lib
    (cd vendor/lib && tick && echo x >x.txt && g add x.txt && g commit -q -m local)
    (cd "$ROOT/lib" && tick && echo three >>l.txt && g commit -q -a -m three)
    v submodule update --merge --remote vendor/lib
    sm_snap vendor/lib
    v submodule update --merge -N vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    (cd "$ROOT/lib" && tick && echo four >>l.txt && g commit -q -a -m four)
    v submodule update -q --merge --remote vendor/lib
    sm_snap vendor/lib
    v submodule update --merge --checkout -N vendor/lib
    g config submodule.vendor/lib.update merge
    (cd "$ROOT/lib" && tick && echo five >>l.txt && g commit -q -a -m five)
    v submodule update --remote vendor/lib
    sm_snap vendor/lib
    (cd vendor/lib && tick && echo a >>l.txt && g commit -q -a -m c1)
    (cd "$ROOT/lib" && tick && echo b >>l.txt && g commit -q -a -m c2)
    v submodule update --merge --remote vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    g -C vendor/lib merge --abort
    (cd "$ROOT/lib" && tick && echo d >>l.txt && g commit -q -a -m c3)
    v submodule update -q --merge --remote vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
}

# `submodule update --rebase`: run `git rebase <recorded commit>` inside the submodule instead of detaching.
s_submodule_update_rebase() {
    sm_nested_top
    g clone -q top clone
    cd clone
    v submodule update --init vendor/lib
    g -C vendor/lib checkout -q -b work
    (cd "$ROOT/lib" && tick && echo two >>l.txt && g commit -q -a -m two)
    v submodule update --rebase --remote vendor/lib
    sm_snap vendor/lib
    (cd vendor/lib && tick && echo x >x.txt && g add x.txt && g commit -q -m local)
    (cd "$ROOT/lib" && tick && echo three >>l.txt && g commit -q -a -m three)
    v submodule update --rebase --remote vendor/lib
    sm_snap vendor/lib
    v submodule update --rebase -N vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    (cd "$ROOT/lib" && tick && echo four >>l.txt && g commit -q -a -m four)
    v submodule update -q --rebase --remote vendor/lib
    sm_snap vendor/lib
    v submodule update --rebase --checkout -N vendor/lib
    g config submodule.vendor/lib.update rebase
    (cd "$ROOT/lib" && tick && echo five >>l.txt && g commit -q -a -m five)
    v submodule update --remote vendor/lib
    sm_snap vendor/lib
    (cd vendor/lib && tick && echo a >>l.txt && g commit -q -a -m c1)
    (cd "$ROOT/lib" && tick && echo b >>l.txt && g commit -q -a -m c2)
    v submodule update --rebase --remote vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    g -C vendor/lib rebase --abort
    g -C vendor/lib checkout -q --detach
    v submodule update -q --rebase -N vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
}

s_submodule_update_depth() {
    sm_nested_top
    g clone -q top clone
    cd clone
    v submodule update --init --depth 1 vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    v submodule deinit -q -f vendor/lib
    v submodule update --init -q --depth=2 -j 2 vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    v submodule update --init -j2 other
    echo "rc=$?"
    v submodule update --depth abc
    echo "rc=$?"
    v submodule update --jobs=x
    echo "rc=$?"
    v submodule update --depth 1k --jobs 1
    echo "rc=$?"
    v submodule update -j
    echo "rc=$?"
}

# `submodule update --single-branch` / `--no-single-branch`: the clone's fetch refspec names one branch or all.
s_submodule_update_single_branch() {
    sm_nested_top
    g clone -q top clone
    cd clone
    v submodule update --init --single-branch vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    g -C vendor/lib config --get-all remote.origin.fetch
    v submodule deinit -q -f vendor/lib
    v submodule update --init -q --depth 1 --no-single-branch vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    g -C vendor/lib config --get-all remote.origin.fetch
    v submodule update --init --single-branch --no-single-branch other
    echo "rc=$?"
    g -C other config --get-all remote.origin.fetch
}

# `submodule update --reference` / `--dissociate`: the module's `objects/info/alternates`, and the failed-clone retry.
s_submodule_update_reference() {
    sm_nested_top
    g clone -q --bare "$ROOT/lib" "$ROOT/refrepo.git"
    g clone -q top clone
    cd clone
    v submodule update --init --reference "$ROOT/refrepo.git" vendor/lib
    echo "rc=$?"
    sm_snap vendor/lib
    sed "s#$ROOT##" .git/modules/vendor/lib/objects/info/alternates
    v submodule update --init --reference="$ROOT/lib" --dissociate other
    echo "rc=$?"
    ls .git/modules/other/objects/info/alternates 2>&1 | sed 's#.*/##'
    v submodule deinit -q -f vendor/lib
    rm -rf .git/modules/vendor
    v submodule update --init --reference /nonexistent vendor/lib
    echo "rc=$?"
    ls .git/modules
    v submodule update --reference
    echo "rc=$?"
}

# `diff --color` / `log -p --color`: bold headers, cyan hunks, red/green lines, red trailing whitespace.
s_diff_color() {
    v init -q repo; cd repo
    printf 'int main() {\n x;\n y;\n z;\n}\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nlast' >f
    printf 'bin\0' >b
    echo gone >g
    g add .; tick; g commit -q -m one
    printf 'int main() {\n x;\n y2  \n z;\n}\nl1\nl2\nl3\nl4\nl5\nl6\nl7\n\nl8\nlast2' >f
    printf 'bin\0x' >b
    rm g
    printf 'new\n\n' >n
    chmod +x n
    v diff --color
    v add -A
    v diff --color --cached
    v diff --color=always --cached
    v diff --cached --color=never
    v diff --color=always --cached --no-color
    v diff --color=sometimes --cached
    echo "rc=$?"
    v diff --color=always --cached --name-status
    tick; g commit -q -m two
    v log --color=always -p
    v log --color=always -p --oneline
    v diff --color=always HEAD~1 HEAD -- f
    v diff --color=always HEAD~1 HEAD --name-only
}

# `status` of a submodule checked out at another commit: ` M path` / `modified:   path (new commits)`.
s_status_moved_submodule() {
    g config --global protocol.file.allow always
    g init -q lib; (cd lib && tick && echo 1 >f && g add f && g commit -q -m l1 && tick && echo 2 >>f && g commit -q -a -m l2)
    g init -q repo; cd repo
    tick; echo t >t; g add t; g commit -q -m t1
    g submodule add -q ../lib sub; g submodule add -q ../lib dir/sub2
    tick; g commit -q -m subs
    v status; v status --short; v status --porcelain
    (cd sub && g checkout -q HEAD~1)
    v status; v status --short; v status --porcelain; v status -s -- sub; v status -s -- dir
    (cd dir/sub2 && g checkout -q HEAD~1)
    v status; v status --short
    g add sub
    v status; v status --short
    (cd sub && g checkout -q -)
    v status; v status --short
}

# `status` of a submodule with a dirty tree: `(modified content)`, `(untracked content)`, both, and
# combined with `new commits`; short codes ` m`, ` ?`, ` M`; the "commit or discard" hint line.
s_status_dirty_submodule() {
    g config --global protocol.file.allow always
    g init -q lib; (cd lib && tick && echo 1 >f && g add f && g commit -q -m l1 && tick && echo 2 >>f && g commit -q -a -m l2)
    g init -q repo; cd repo
    tick; echo t >t; g add t; g commit -q -m t1
    g submodule add -q ../lib sub
    tick; g commit -q -m subs
    v status
    echo x >sub/f
    v status; v status --short; v status --porcelain
    echo n >sub/new
    v status; v status --short
    (cd sub && g checkout -q -- f)
    v status; v status --short; v status --porcelain
    rm sub/new
    echo s >sub/staged; (cd sub && g add staged)
    v status; v status --short
    (cd sub && g reset -q --hard)
    echo n >sub/new
    (cd sub && g checkout -q HEAD~1)
    v status; v status --short
    echo y >sub/f
    v status; v status --short
    v status -s -- sub
}

# `status -- <file>`: a pathspec that names a file (tracked, untracked, in a subdirectory, absent).
s_status_file_pathspec() {
    g init -q repo; cd repo
    tick; echo a >f; mkdir d; echo b >d/g; g add .; g commit -q -m c1
    echo c >>f; echo n >new; echo m >d/new2; echo o >d/h
    v status -- f; v status -s -- f; v status -s -- new; v status -s -- d/new2
    v status -s -- d/g; v status -s -- nope; v status --porcelain -- d/h
    rm d/g; v status -s -- d; v status -s -- d/g; v status -s -- .; v status -- d
}

# `status` with several pathspecs: the union of the matches, the hints for that union, `--` and
# options mixed in, and `--porcelain` not swallowing the first pathspec as its version.
s_status_multi_pathspec() {
    g init -q repo; cd repo
    tick; mkdir a b c; echo 1 >a/x; echo 1 >b/y; echo 1 >c/z; echo 1 >top; g add .; g commit -q -m c1
    echo 2 >>a/x; echo 2 >>b/y; echo 2 >>c/z; echo n >a/new; echo n >c/new; g add b/y
    v status a b; v status a c/z; v status -- a b; v status a/new top; v status nomatch a
    v status b a/x; v status a a/x; v status . a
    v status --short a c; v status -s -- b c; v status -b -s a/x c; v status -uno a c
    v status --porcelain a c; v status --porcelain=v1 a c; v status --porcelain -- b c
    v status -s a nomatch; v status -s nomatch other
    cd a; v status -s x ../b; v status . ../c; v status -- ../b/y new
}

# `status -- <dir>` where the directory is wholly untracked: git reports `?? dir/`, not the files
# inside; a tracked parent, a trailing slash, `-uall`, a second pathspec and a nested untracked
# directory under a tracked one all follow the same rule.
s_status_untracked_dir_pathspec() {
    g init -q repo; cd repo
    tick; echo a >f; mkdir t; echo b >t/g; g add .; g commit -q -m c1
    mkdir -p d/e t/u; echo 1 >d/a; echo 2 >d/e/b; echo 3 >t/u/k; echo 4 >t/new; echo n >x
    v status -s -- d; v status -s -- d/; v status -s -- d/e; v status -s -- t/u; v status -s -- t
    v status -s -uall -- d; v status -s -- d x; v status -s -- .; v status -- d; v status --porcelain d/e
    mkdir empty; v status -s -- empty
}

# `status` with pathspec magic and wildcards: `:(exclude)`/`:!`/`:^`, `:(top)`/`:/`, `:(glob)`,
# `:(literal)`, `:(icase)`, a bare `*` that crosses `/`, untracked directories that collapse only
# when the whole directory matches, exclude-only lists, `..` in patterns and git's errors.
s_status_pathspec_magic() {
    g init -q repo; cd repo
    tick; mkdir -p d/e u; echo 1 >a.txt; echo 1 >d/b.c; echo 1 >d/e/c.c; echo 1 >d/e/x.txt
    g add .; g commit -q -m c1
    echo 2 >>a.txt; echo 2 >>d/b.c; echo 2 >>d/e/c.c; echo 2 >>d/e/x.txt; g add d/b.c
    echo n >new.c; echo n >d/new.c; echo n >u/a.c; echo n >u/b.txt; echo n >u/c.c
    for s in ':(exclude)d' ':!d/e' ':^d' '*.c' '*.txt' ':(glob)**/*.c' ':(glob)*.c' ':(glob)u/*.c' \
        ':(top)a.txt' ':/d' ':(icase)A.TXT' ':(literal)*.c' ':!*.txt' ':(exclude)u/b.txt' 'u/*' \
        ':!u' ':(top)u' 'd/*.c' 'd/e' ':(foo)x' ':(top' ':(glob,literal)x' ':#x' ':(exclude,icase)D'; do
        v status -s -- "$s"; v status -- "$s"
    done
    v status -s '*.c' ':!d'; v status -s ':!d' ':!u'; v status -s -uall ':!*.txt'; v status -s -uno '*.c'
    v status --porcelain ':(glob)u/*'
    cd d
    for s in ':(top)a.txt' ':/a.txt' ':(exclude)e' ':!new.c' ':(top)d/new.c' '../*.c' ':(icase)NEW.C' \
        '*.c' ':(glob)*.c' '../../*.c'; do
        v status -s -- "$s"; v status -- "$s"
    done
    v status -s ':(top)a.txt' '*.c'; v status -s ':!e' ':!new.c'
}

# A plain pathspec that climbs out of the work tree gets git's `outside repository at` error.
s_status_outside_pathspec() {
    g init -q repo; cd repo
    tick; mkdir d; echo 1 >d/f; g add .; g commit -q -m c1
    v status -- ../x; v status -s ../../x a ../y; v status -- d/../../x
    cd d
    v status -- ../../x; v status -s ../f '../../y'
}

s_ls_remote_patterns() {
    g init -q src; cd src
    tick; echo a >f; g add .; g commit -q -m c1
    for b in a1 a2 b1 B1 x-y 'ab]c' 'e!f'; do g branch "$b"; done
    g tag v1.0
    for p in 'refs/heads/[ab]1' '*/[a-b]1' '*/[!a]1' '*/[^a]1' '*/[[:upper:]]1' '*/[[:digit:]]' \
        '*/a[1-2]' '*/ab[]]c' '*/e\!f' '*/a\1' '*/a[1' '*/a[]' '*/[x-]y' '*v1.[0]' '*/[[:nope:]]1'; do
        v ls-remote --heads --tags ../src "$p"
    done
    v ls-remote ../src 'refs/heads/[ab]1' 'refs/tags/v1.[0]'
}

s_ls_remote_sort() {
    g init -q src; cd src
    tick; echo a >f; g add .; g commit -q -m c1
    for b in zeta Alpha beta; do g branch "$b"; done
    for t in v1 v10 v2 a; do g tag "$t"; done
    tick; g tag -a -m ann ann
    for k in refname -refname refname:strip=1 refname:lstrip=2 refname:lstrip=-1 refname:rstrip=1 \
        refname:rstrip=-1 -refname:lstrip=9 refname:strip=0 refname: -refname:rstrip=2 \
        nokey '' - refname:foo refname:lstrip refname:lstrip=x refname:rstrip= ' refname' 'refname,objectname'; do
        v ls-remote --sort="$k" ../src
    done
    v ls-remote --sort=-refname --sort=refname:lstrip=2 ../src
    v ls-remote --sort=refname:lstrip=2 --sort=-refname:rstrip=1 ../src
    v ls-remote --heads --sort=-refname ../src
    v ls-remote --tags --refs --sort=-refname ../src
    v ls-remote --symref --sort=-refname ../src
    v ls-remote --sort=-refname ../src 'v*'
    v ls-remote --sort=-refname --exit-code ../src nomatch
    v ls-remote --sort=nokey ../nonexistent
    cd ..; g init -q other; cd other
    v ls-remote --sort=-refname ../src
}

# `ls-remote --sort=version:<key>` / `v:<key>` use git's versioncmp, reversed by a leading `-`.
s_ls_remote_version_sort() {
    g init -q src; cd src
    tick; echo a >f; g add .; g commit -q -m c1
    g branch v1.100
    for t in v1.9 v1.10 v1.2 v1.09 a1 a01 v2; do g tag "$t"; done
    tick; g tag -a -m ann v3
    for k in version:refname -version:refname v:refname -v:refname v:refname:lstrip=2 \
        version:refname:strip=-1 v:refname:rstrip=1 version: v: version:foo version v:refname:foo \
        v:refname:lstrip=x -version:; do
        v ls-remote --sort="$k" ../src
    done
    v ls-remote --sort=v:refname --sort=-refname:strip=2 ../src
    v ls-remote --sort=refname --sort=-v:refname ../src
    v ls-remote --tags --sort=-v:refname ../src 'v1*'
}

# `ls-remote --sort=version:` honours `versionsort.suffix` and the deprecated `prereleasesuffix`.
s_ls_remote_version_suffix() {
    g init -q src; cd src
    tick; echo a >f; g add .; g commit -q -m c1
    for t in v1.0 v1.0-rc1 v1.0-rc2 v1.0-rc10 v1.0-beta1 v1.0-foo v1.0.1 v1.1-beta v1.1 v0.9 v1.0-pre-rc1; do
        g tag "$t"
    done
    v ls-remote --tags --sort=version:refname ../src
    g config --add versionsort.suffix -rc
    v ls-remote --tags --sort=version:refname ../src
    v ls-remote --tags --sort=-v:refname ../src
    g config --add versionsort.suffix -beta
    v ls-remote --tags --sort=v:refname ../src
    g config --unset-all versionsort.suffix
    g config --add versionsort.suffix -beta
    g config --add versionsort.suffix -pre
    g config --add versionsort.suffix -rc
    v ls-remote --tags --sort=version:refname ../src
    g config --unset-all versionsort.suffix
    g config --add versionsort.prereleasesuffix -rc
    v ls-remote --tags --sort=version:refname ../src
    v ls-remote --tags --sort=refname ../src
}

# `sparse-checkout set` over a skip-worktree path whose file reappeared with other content.
s_sparse_reappeared() {
    v init -q repo; cd repo
    mkdir a b; echo 1 >a/f; echo 2 >b/f; echo 3 >b/g; echo 4 >top
    v add .; v commit -q -m one
    v sparse-checkout set --cone a
    mkdir b; echo other >b/f; echo 2 >b/g
    v sparse-checkout set b; echo "rc=$?"
    v ls-files -t; cat b/f b/g; v status -s
    v sparse-checkout set a; echo "rc=$?"
    v ls-files -t; ls b; v status -s
    mkdir -p b; echo other >b/f
    v sparse-checkout reapply; echo "rc=$?"
    v ls-files -t; v status -s
}

# `show --color[=<when>]` / `--no-color`: commit and tag headings, the patch, and the refusals.
s_show_color() {
    v init -q repo; cd repo
    printf 'a\nb  \n' >f
    echo gone >g
    g add .; tick; g commit -q -m one
    printf 'a\nc  \n' >f
    rm g
    printf 'new\n' >n
    chmod +x n
    g add -A; tick; g commit -q -m two
    tick; g tag -a -m tagmsg v1
    g tag light
    v show --color=always
    v show --color
    v show --color=never
    v show --no-color
    v show --color=always --no-color
    v show --no-color --color=always
    v show --color=bogus
    echo "rc=$?"
    v show --color=always -s
    v show --color=always --pretty=oneline
    v show --color=always --format='%h %d %s'
    v show --color=always --name-only
    v show --color=always --name-status
    v show --color=always -U1 -- f
    v show --color=always HEAD~1
    v show --color=always v1
    v show --color=always light
    v show --color=always 'HEAD^{tree}'
    v show --color=always HEAD:f
    g config color.ui always
    v show
    v show --no-color
    v show --color=never
    g config color.ui false
    v show --color=always -s
}

# `--stat` under colour: `log`, `show` and `diff` paint the graph (`+` green, `-` red) and the binary
# sizes (old red, new green); the summary line stays plain.
s_log_stat_color() {
    v init -q repo; cd repo
    seq 1 30 >f
    echo gone >g
    printf 'a\0b\n' >bin
    printf 'x\n' >mode
    g add .; tick; g commit -q -m one
    seq 1 5 >f
    rm g
    echo n >n
    printf 'a\0cc\n' >bin
    echo 'bb b' >'sp ace'
    chmod +x mode
    g add -A; tick; g commit -q -m two
    printf '%s\n' "$(seq 1 200)" >long
    echo more >>f
    g add -A; tick; g commit -q -m three
    g checkout -q -b side HEAD~1
    echo s >side; g add side; tick; g commit -q -m side
    g checkout -q master 2>/dev/null || g checkout -q main
    tick; g merge -q --no-edit side
    v log --color=always --stat
    v log --color=always --stat -2 --oneline
    v log --color=always --stat --format=%s
    v log --color=never --stat -1
    v log --stat --color=always --no-color -1
    v log --color=always --stat -- f
    v show --color=always --stat
    v show --color=always --stat HEAD~1
    v show --color=always --stat HEAD~2
    v show --color=always --stat --format=%h
    v show --no-color --stat
    v diff --color=always --stat HEAD~3 HEAD~2
    v diff --color=always --stat HEAD~3 HEAD
    v diff --color=always --stat HEAD HEAD
    echo more >>f; echo nn >n
    v diff --color=always --stat
    g add -A
    v diff --color=always --stat --cached
    g config color.ui always
    v diff --stat
    v log --stat -1
    g config color.ui false
    v log --color=always --stat -1
}

# `blame --incremental`: groups in the order their commits settle, details once per commit, `previous`
# and `filename` for every group, no source lines.
s_blame_incremental() {
    v init -q repo; cd repo
    printf 'a\nb\nc\nd\n' >f
    v add f; tick; v commit -q -m one
    printf 'a\nB\nc\nd\ne\n' >f
    GIT_AUTHOR_NAME="Bob" GIT_AUTHOR_EMAIL=bob@x v commit -q -a -m two
    tick; git mv f g; printf 'a\nB\nC\nd\ne\n' >g
    v commit -q -a -m three
    v blame --incremental g
    v blame --incremental -L2,3 g
    v blame --incremental -L5 g
    v blame --incremental HEAD~1 -- f
    v blame --incremental HEAD~2 f
    v blame --incremental nosuch
    echo "rc=$?"
}

# `blame -w`: lines that differ only in whitespace stay with the earlier commit; without `-w` they
# move to the reindenting commit.
s_blame_ignore_whitespace() {
    v init -q repo; cd repo
    printf 'int main() {\nfoo(1);\nbar(2);\n\nreturn 0;\n}\n' >f
    v add f; tick; v commit -q -m one
    printf 'int main() {\n    foo(1);\n\tbar(2);\n\n    return 0;\n}\n' >f
    GIT_AUTHOR_NAME="Bob" GIT_AUTHOR_EMAIL=bob@x v commit -q -a -m reindent
    tick; printf 'int main() {\n    foo( 1 );\n\tbar(2);\n    baz();\n\n    return 0;\n}\n' >f
    v commit -q -a -m three
    v blame f
    v blame -w f
    v blame -w -L2,3 f
    v blame -w --porcelain f
    v blame -w --incremental f
    v blame -w HEAD~1 -- f
}

# `blame -L /re/`: a start regex searches from line 1 (or from the line after the previous `-L`),
# `^/re/` from line 1, an end regex from the line after the start; no match, bad regex, unterminated
# regex and `+0` die or print usage as git does.
s_blame_regex_range() {
    v init -q repo; cd repo
    printf 'alpha\nbeta\ngamma\nbeta2\ndelta\nepsilon\n' >f
    v add f; tick; v commit -q -m one
    printf 'a\nb\nb\nb\nz\n' >g
    v add g; tick; v commit -q -m two
    local r
    for r in '/beta/' '/beta/,/delta/' '2,/beta/' '3,/beta/' '/beta/,+2' '/beta/,-1' '^/beta/' '^/beta/,/delta/' \
        '/beta/,3' '/beta/,1' '/beta/,' ',/gamma/' '/^g/' '/b.t[a]/' '/BETA/' '//' '/beta/,//' '/b\/x/' \
        '/zzz/' '2,/zzz/' '/beta/,/alpha/' '5,/beta/' '6,/beta/' '7,/beta/' '/(/' '1,/\(/' '/beta/,+0' \
        '/beta' '/beta/x' '/beta/,x' '2,/beta/x' '3,^/beta/' '0,/beta/'; do
        echo "-L $r"
        v blame -L "$r" f
        echo "rc=$?"
    done
    v blame -L /b/ -L /b/ g
    echo "rc=$?"
    v blame -L /b/,+1 -L /b/,+1 g
    echo "rc=$?"
    v blame -L 2,2 -L /b/ g
    echo "rc=$?"
    v blame -L '^/b/' -L '^/z/' g
    echo "rc=$?"
}

# `log --first-parent` diffs a merge against its first parent (stat, patch, name lists); an empty
# `--format=` / `--pretty=` prints no header, no terminator and no blank line before the diff.
s_log_first_parent_merge_diff() {
    v init -q repo; cd repo
    echo a >a; v add a; tick; v commit -q -m base
    g checkout -q -b side
    echo b >b; v add b; tick; v commit -q -m side
    g checkout -q master 2>/dev/null || g checkout -q main
    echo c >c; v add c; tick; v commit -q -m main2
    tick; g merge -q --no-ff side -m merge
    v log --first-parent --stat --format=%s
    v log --first-parent -p --format=%s
    v log --first-parent --name-only --format=%s
    v log --first-parent --name-status --format=%s
    v log --first-parent --stat
    v log --stat --format=%s
    v show --format= --stat HEAD
    v show --format= HEAD~1
    v show --pretty= HEAD~1
    v log --format= -p
    v log --pretty= --stat
}

# The last of `--oneline`, `--pretty=` and `--format=` wins; `--oneline` abbreviates the `commit` line
# of whatever layout follows it, `--pretty=oneline` alone does not.
s_log_layout_order() {
    v init -q repo; cd repo
    echo a >a; v add a; tick; v commit -q -m one
    echo b >b; v add b; tick; v commit -q -m two
    v log --format= --oneline
    v log --oneline --format=
    v log --oneline --format=%s
    v log --format=%s --oneline
    v log --pretty=short --oneline
    v log --oneline --pretty=full
    v log --pretty=oneline --oneline
    v log --oneline --pretty=oneline
    v log --format=%s --pretty=full
    v log --pretty=full --format=%s
    v log --format=%s --pretty=oneline
    v log --pretty=oneline --format=%s
    v log --oneline --pretty=medium -- a
}

# Hex-looking names: git tries refs before short object ids, and an abbreviation is matched nibble by
# nibble (`xx00` is not `xx`). Fixture is built by real git; the ambiguous pair is found by hashing.
s_revparse_hexnames() {
    g init -q repo; cd repo
    echo a >a; g add a; tick; g commit -qm one
    echo b >b; g add b; tick; g commit -qm two
    g branch cafe HEAD~1; g branch facade HEAD
    first=$(g rev-parse HEAD | cut -c1-4)
    g branch "$first" HEAD~1
    v rev-parse cafe facade
    v rev-parse "$first"
    v rev-parse --verify "$first^{commit}"
    v log --oneline cafe
    v log --oneline facade
    v show -s --format=%s cafe
    v show -s --format=%s "$first"
    v show -s --format=%s "$(g rev-parse HEAD | cut -c1-5)"
    v show -s --format=%s "$(g rev-parse HEAD | cut -c1-8)"
    zero="$(g rev-parse HEAD | cut -c1-2)00"
    v rev-parse "$zero"
    v rev-parse "$(g rev-parse HEAD | cut -c1-2)0"
    mkdir ../blobs
    for i in $(seq 1 2500); do echo "blob $i" >"../blobs/$i"; done
    seq 1 2500 | (cd ../blobs && g hash-object --stdin-paths) >../ids
    pre=$(cut -c1-4 ../ids | sort | uniq -d | head -1)
    for i in $(grep -n "^$pre" ../ids | cut -d: -f1); do g hash-object -w "../blobs/$i" >/dev/null; done
    v rev-parse "$pre"
    v cat-file -t "$pre"
}

# Delta chains deeper than git's default 50: real `git fast-import --depth=200` writes a pack whose
# longest chain is 149 links. Reading it, `fsck` and `log` must match git.
s_deep_delta_chain() {
    g init -q repo; cd repo
    for i in $(seq 1 150); do
        printf 'commit refs/heads/main\ncommitter A <a@b> %d +0000\ndata 4\nc%02d\n' $((1700000000 + i)) $((i % 100))
        printf 'M 100644 inline f\ndata <<E\n'
        seq 1 3000; echo "rev $i"
        printf 'E\n\n'
    done | g fast-import --depth=200 --quiet
    g reset -q --hard
    v fsck
    v log --oneline
    v rev-list --count HEAD
    v cat-file -s HEAD:f
    v cat-file -p HEAD~149:f
    v show -s --format=%s HEAD~100
    v status
    v verify-pack -v .git/objects/pack/pack-*.idx
}

# `rev-parse --short` and the abbreviation rule: git raises the length until no other object shares
# the digits, honours core.abbrev, and takes --short's number only with `=`. 2400 packed blobs plus
# 300 loose ones make several four-digit prefixes ambiguous (verified against git: the scenario
# prints at least one 5-digit result), so the neighbour search over the pack index and the loose
# bucket both matter.
s_abbrev() {
    g init -q repo; cd repo
    {
        printf 'commit refs/heads/main\ncommitter A <a@b> 1700000000 +0000\ndata 4\nabbr\n'
        for i in $(seq 1 2400); do printf 'M 100644 inline f%d\ndata <<E\nblob %d\nE\n' "$i" "$i"; done
        printf '\n'
    } | g fast-import --quiet
    g reset -q --hard
    for i in $(seq 1 300); do printf 'loose %d\n' "$i" | g hash-object -w --stdin >/dev/null; done
    v rev-parse --short HEAD
    v rev-parse --short=12 HEAD
    v rev-parse --short=0 HEAD
    v rev-parse --short=3 HEAD
    v rev-parse --short=99 HEAD
    v rev-parse --short HEAD~0 HEAD
    v rev-parse --short 5 HEAD
    v log --oneline
    g config core.abbrev 10
    v rev-parse --short HEAD
    v log --oneline
    v rev-list --abbrev-commit HEAD
    g config core.abbrev no
    v rev-parse --short HEAD
    v log --oneline
    g config core.abbrev 4
    v rev-parse --short HEAD
    g config core.abbrev auto
    v rev-parse --short HEAD
    g config --unset core.abbrev
    local oid n=0 longer=0
    for oid in $(g cat-file --batch-all-objects --batch-check='%(objectname)' | sort | head -700); do
        v rev-parse --short=4 "$oid"
        if [ "$(g rev-parse --short=4 "$oid" | wc -c)" -gt 5 ]; then longer=$((longer + 1)); fi
        n=$((n + 1))
    done
    [ "$longer" -gt 0 ] || { echo "fixture has no ambiguous 4-digit prefix"; return 1; }
}

# Unknown `extensions.*` in a version-1 repository are refused with git's list (config order, repeats,
# lower-cased keys, `sub.q` for a subsection); version 0 ignores them. `extensions.preciousObjects`
# makes `gc` and `repack -d` leave every pack and loose object in place.
s_repo_extensions() {
    g init -q repo; cd repo
    echo a >a; g add a; tick; g commit -qm one
    local ver body
    while IFS='|' read -r ver body; do
        printf '[core]\n\trepositoryformatversion = %s\n\tbare = false\n[extensions]\n%b\n' "$ver" "$body" >.git/config
        echo "--- v$ver $body"
        v status --short
        v log --oneline
        v rev-parse HEAD
    done <<'EOF2'
1|\tfrobnicate = true
0|\tfrobnicate = true
1|\tFrobnicate = false
1|\tzzz = 1\n\tAAA = 2\n\tzzz = 3\n\tnoop = 1
1|\tpreciousObjects = true\n\tworktreeConfig = true\n\tobjectFormat = sha1\n\trefStorage = files
1|\tpartialClone = origin\n\tcompatObjectFormat = sha256
EOF2
    printf '[core]\n\trepositoryformatversion = 1\n[extensions "sub"]\n\tQ = 1\n' >.git/config
    v status --short
    # preciousObjects: nothing is ever deleted
    printf '[core]\n\trepositoryformatversion = 1\n\tbare = false\n[extensions]\n\tpreciousObjects = true\n' >.git/config
    echo b >b; g add b; tick; g commit -qm two
    g reset -q --hard HEAD~1
    g repack -q
    g reflog expire --expire=now --all
    junk=$(echo junk | g hash-object -w --stdin)
    lost=$(g rev-parse HEAD)
    v gc --prune=now
    echo "gc rc=$?"
    v cat-file -t "$junk"
    v repack -a -d
    echo "repack -a -d rc=$?"
    v repack -d
    echo "repack -d rc=$?"
    v cat-file -t "$junk"
    v count-objects -v
    v fsck
    ls .git/objects/pack | sed 's/pack-[0-9a-f]*/pack-X/' | sort | uniq -c
}

# `git clone --shared` leaves `objects/info/alternates` and no objects of its own: every read command
# must find the history through it, including a relative alternate and a chained one.
# Put the bytes of the given files into the transcript, so a conversion difference is readable.
dump_bytes() {
    local f
    for f in "$@"; do echo "bytes $f:$(od -An -c "$f" | tr -s ' ' | tr -d '\n')" >>"$TRANSCRIPT"; done
}

s_attributes_eol() {
    g init -q repo; cd repo
    printf '* text=auto\n*.txt eol=crlf\n*.bin -text\n*.lf eol=lf\n*.in text=auto eol=lf\n' >.gitattributes
    mkdir sub
    printf 'sub/*.dat -text\n' >sub/.gitattributes
    printf 'a\r\nb\r\n' >crlf.dat; printf 'a\nb\n' >lf.dat; printf 'a\r\nb\n' >mixed.dat
    printf 'a\0\r\nb\r\n' >nul.dat; printf 'a\rb\n' >lonecr.dat
    printf 'a\r\nb\r\n' >w.txt; printf 'a\nb\n' >l.txt; printf 'a\r\n' >x.bin
    printf 'a\r\nb\r\n' >e.lf; printf 'a\r\nb\r\n' >e.in; printf 'a\r\nb\r\n' >sub/s.dat
    v add .
    v ls-files -s
    v status --short
    tick; g commit -qm one
    local files="crlf.dat lf.dat mixed.dat nul.dat lonecr.dat w.txt l.txt x.bin e.lf e.in sub/s.dat"
    local c
    for c in false true input; do
        g config core.autocrlf $c
        rm -f $files
        v checkout -- .
        dump_bytes $files
        v status --short
        vq add -A
        v ls-files -s
    done
    # the safe-crlf diagnostics, and the refusal when it is set to true
    g config core.autocrlf false
    printf 'a\r\n' >new.dat; printf 'a\nb\n' >q.txt
    v add new.dat q.txt
    g config core.safecrlf true
    printf 'a\r\n' >new2.dat
    printf 'a\nb\n' >q2.txt
    v add new2.dat
    v add q2.txt
    g config core.safecrlf false
    v add new2.dat
    v ls-files -s
    cd ..
    # no attributes at all: core.autocrlf decides, and an index blob that already holds a CR is kept
    g init -q plain; cd plain
    printf 'a\r\nb\r\n' >old.dat; printf 'x\r\n' >keep.dat
    v add .
    tick; g commit -qm one
    for c in true input false; do
        g config core.autocrlf $c
        printf 'a\r\nb\r\nc\r\n' >old.dat; printf 'n\r\n' >fresh.dat
        v add old.dat fresh.dat
        v ls-files -s
        v status --short
        v rm -q --cached fresh.dat; rm -f fresh.dat
        v reset -q --hard
        dump_bytes old.dat keep.dat
    done
    printf '* text=auto\n' >.gitattributes
    printf 'x\r\ny\r\n' >keep.dat
    v add keep.dat
    v ls-files -s
}

s_alternates() {
    g init -q repo; cd repo
    echo a >a; g add a; tick; g commit -qm one
    echo b >b; g add b; tick; g commit -qm two
    g tag -a -m rel v1
    cd ..
    g clone -q --shared repo cl
    g clone -q --shared cl cl2
    mkdir rel; g init -q rel/r; mkdir -p rel/r/.git/objects/info
    printf '# comment\n\n../../../../repo/.git/objects\n' >rel/r/.git/objects/info/alternates
    cd cl
    v log --oneline
    v status
    v rev-parse HEAD HEAD~1 'HEAD^{tree}' v1
    v cat-file -t HEAD
    v cat-file -s HEAD:b
    v cat-file -p HEAD:a
    v show --stat HEAD
    v count-objects
    echo c >c; g add c; tick; g commit -qm three
    v log --oneline
    v fsck
    cd ../cl2
    v log --oneline
    v status
    v rev-parse --verify v1
    v show HEAD~1:a
    cd ../rel/r
    v cat-file -t "$(g -C ../../repo rev-parse HEAD)"
    v cat-file -p "$(g -C ../../repo rev-parse 'HEAD^{tree}')"
    short=$(g -C ../../repo rev-parse --short=10 HEAD)
    v cat-file -t "$short"
}

# A clone made by real git with `--depth` lists its boundary commits in `.git/shallow`; the parents of
# those commits are absent by design, so every walker must stop there instead of failing.
s_shallow() {
    g init -q src; cd src
    for i in 1 2 3 4 5; do echo $i >f$i; g add .; tick; g commit -qm "c$i"; done
    cd ..
    g clone -q --depth=2 "file://$PWD/src" cl
    cd cl
    v log --oneline
    v log --graph --oneline
    v log --format='%h %P'
    v show -s --format=%P HEAD~1
    v rev-list HEAD
    v rev-list --count HEAD
    v rev-list --parents HEAD
    v rev-list HEAD~1..HEAD
    v merge-base HEAD HEAD~1
    v rev-parse 'HEAD~1^'
    v blame f5
    v describe --always
    v status
    v fsck
    echo 6 >f6; g add f6; tick; g commit -qm c6
    v log --oneline
    v fsck
}

s_malformed_ident() {
    g init -q r; cd r
    local t p="" c a
    t=$(g hash-object -t tree -w /dev/null)
    for a in "Foo 1234 +0000" "Foo <foo@x.com>" "Foo <foo@x.com> 99999999999999999999999 +0000" \
             "Foo <foo@x.com> 1234 +00000" "Foo <foo@x.com> 1234 +0000"; do
        c=$(printf 'tree %s\n%sauthor %s\ncommitter C <c@x> 1234 +0000\n\nmsg\n' "$t" "$p" "$a" \
            | g hash-object -t commit -w --stdin --literally)
        p="parent $c
"
    done
    g update-ref refs/heads/master "$c"
    g symbolic-ref HEAD refs/heads/master
    v log --oneline
    v log --format='%h|%an|%ae|%cn'
    v log --format='%h|%ad' -3
    v log --format='%H %aI' -2
    v rev-list --count HEAD
    v show -s --format='%an <%ae> %at' HEAD~1
    v show -s --format='%an <%ae>' HEAD~3
    v fsck
}

# A8 remainder: an identity line with no `<email>` or no dated `+zone` is not rendered as if the
# part were there (no `Author:`/`Date:` line, empty or unexpanded `%a?` placeholders).
s_malformed_ident_render() {
    g init -q r; cd r
    local t p="" c a
    t=$(g hash-object -t tree -w /dev/null)
    for a in "Foo 1234 +0000" "Foo <foo@x.com>" "Foo <foo@x.com> 99999999999999999999999 +0000" \
             "Foo <foo@x.com> 1234 +00000" "Foo <foo@x.com> 1234" "Foo <foo@x.com> 1234 +0000"; do
        c=$(printf 'tree %s\n%sauthor %s\ncommitter C <c@x> 1234 +0000\n\nmsg\n' "$t" "$p" "$a" \
            | g hash-object -t commit -w --stdin --literally)
        p="parent $c
"
    done
    # a committer with no date, to cover the `%c` side and the Commit/CommitDate lines
    c=$(printf 'tree %s\nparent %s\nauthor A <a@x> 5 +0100\ncommitter Nobody 7 +0000\n\nlast\n' "$t" "$c" \
        | g hash-object -t commit -w --stdin --literally)
    g update-ref refs/heads/master "$c"
    g symbolic-ref HEAD refs/heads/master
    local f
    for f in short medium full fuller; do
        v log --pretty=$f
    done
    v log --format='%h|%an|%ae|%ad|%at|%aI|%aD|%ar|%as|%ai|%aN|%aE'
    v log --format='%h|%cn|%ce|%cd|%ct|%cI|%cD|%cs|%cN|%cE'
    v log --format='%h|%ad|%at' --date=iso
    v log --format='%h|%ad|%at' --date=raw
    v log --format='%h|%ad' --date=short
    v show -s --format='%an|%ae|%ad|%aI' HEAD~2
    v fsck
}

# `feature.manyFiles` / `index.version=4` make git write the prefix-compressed v4 index; rivet must read
# it back path for path (and, with no way to write v4, still leave an index git reads).
s_index_v4() {
    g init -q repo; cd repo
    mkdir -p src/deep/er lib
    local i
    for i in 1 2 3; do echo "$i" >"src/deep/er/file$i.txt"; echo "$i" >"src/deep/mod$i.rs"; done
    echo top >README; echo x >src/a; echo y >lib/z; echo w >lib/zz
    printf 'x\n' >'src/deep/er/f with space'
    g add .; tick; g commit -qm one
    g config index.version 4
    g update-index --index-version 4
    echo 4 >src/deep/er/file4.txt; g add src/deep/er/file4.txt
    v ls-files -s
    v ls-files
    v status
    v diff --stat
    echo 5 >>src/a
    v status --short
    v diff
    v ls-files -m
    v write-tree
    tick; v commit -qam two
    v log --oneline
    v ls-files -s
    v fsck
}

# Criss-cross history built by real git: the merge bases of main and side are x1 and y1, whose
# virtual merge decides line 8. Merged against either base alone, one direction would conflict.
criss_cross_fixture() {
    g init -q -b main repo; cd repo
    printf '1\n2\n3\n4\n5\n6\n7\n8\n9\n' >f; g add f; tick; g commit -q -m base
    g branch side
    tick; sed -i 2s/.*/X2/ f; g commit -q -a -m x1
    g switch -q side
    tick; sed -i 8s/.*/Y8/ f; echo y >y; g add y; g commit -q -a -m y1
    g switch -q main
    tick; g merge -q --no-edit side
    g switch -q side
    tick; g merge -q --no-edit main~1
    g switch -q main
    tick; sed -i 8s/.*/M8/ f; g commit -q -a -m x2
    g switch -q side
    tick; echo y2 >y2; g add y2; g commit -q -m y2
    g switch -q main
    g branch mainb
}

s_merge_criss_cross() {
    criss_cross_fixture
    v merge-base --all main side
    v merge --no-edit side
    v status --short
    cat f
    v ls-files -s
    v log --oneline --graph
    g switch -q side
    v merge --no-edit mainb
    v ls-files -s
    cat f
    v log --oneline
    v fsck
}

s_merge_criss_cross_conflict() {
    criss_cross_fixture
    tick; g switch -q side; sed -i 5s/.*/S5/ f; g commit -q -a -m side5
    g switch -q main; sed -i 5s/.*/M5/ f; tick; g commit -q -a -m main5
    g branch mainb
    v merge --no-edit side
    v status --short
    cat f
    v ls-files -s
    v merge --abort
    g switch -q side
    v merge --no-edit mainb
    cat f
    v ls-files -s
}

s_update_ref() {
    v init -q repo; cd repo
    echo a >a; v add a; tick; v commit -q -m one
    echo b >a; v commit -q -a -m two
    A=$(g rev-parse HEAD~1); B=$(g rev-parse HEAD); Z=0000000000000000000000000000000000000000
    # in <input> <args...>: feed update-ref --stdin from a file
    ur_in() { printf "$1" >.git/stdin.txt; shift; STDIN_FILE=.git/stdin.txt v update-ref "$@"; }
    # positional create, move, stale old, create-only, missing ref with an old
    v update-ref refs/heads/x $A
    v update-ref refs/heads/x $B $B
    v update-ref refs/heads/x $B $A
    v update-ref refs/heads/x $B $B
    v update-ref refs/heads/y $B $Z
    v update-ref refs/heads/y $B $Z
    v update-ref refs/heads/y $B ''
    v update-ref refs/heads/nope $B $A
    v update-ref -m 'a reason' refs/heads/m $A
    v update-ref -m 'moved' refs/heads/m HEAD
    v update-ref refs/heads/m HEAD~1
    v update-ref refs/heads/q nothex
    v update-ref 'bad name' $A
    v update-ref refs/tags/t $A
    v update-ref refs/tags/t2 $A
    v for-each-ref
    # delete: right, wrong, absent old, absent
    v update-ref -d refs/heads/x $A
    v update-ref -d refs/heads/x $B
    v update-ref -d refs/heads/x
    v update-ref -d refs/heads/zz
    v update-ref -d refs/heads/y $B
    v update-ref refs/heads/y $B
    v update-ref refs/heads/y $Z
    v update-ref refs/heads/y2 $B $B
    v update-ref refs/heads/y2 $Z
    v update-ref refs/heads/y2
    # usage errors
    v update-ref
    v update-ref HEAD
    v update-ref -d
    v update-ref -d refs/heads/m $A $B
    v update-ref refs/heads/m $A $B extra
    # through a symbolic ref, and --no-deref
    v update-ref HEAD $A
    v log --oneline
    v update-ref -m back HEAD $B
    v reflog show HEAD
    v reflog show master
    v symbolic-ref refs/heads/alias refs/heads/m
    v update-ref refs/heads/alias $A
    v rev-parse refs/heads/m
    v update-ref --no-deref refs/heads/alias $B
    v symbolic-ref -q refs/heads/alias
    v update-ref --no-deref HEAD $A
    v symbolic-ref -q HEAD
    v update-ref --no-deref HEAD refs/heads/master
    v checkout -q master
    v update-ref --create-reflog refs/tags/forced $A
    v update-ref --create-reflog -m forced refs/tags/forced $B
    # packed refs
    v pack-refs --all
    v update-ref refs/heads/m $B
    v update-ref -d refs/heads/m $B
    v update-ref -d refs/tags/t
    v for-each-ref
    # --stdin, line mode
    ur_in "create refs/heads/a $A\n" --stdin
    ur_in "create refs/heads/a $A\n" --stdin
    ur_in "update refs/heads/a $B $A\n" --stdin
    ur_in "update refs/heads/a $B $A\n" --stdin
    ur_in "update refs/heads/a $A $Z\n" --stdin
    ur_in "verify refs/heads/a $B\n" --stdin
    ur_in "verify refs/heads/a $A\n" --stdin
    ur_in "verify refs/heads/nope\n" --stdin
    ur_in "verify refs/heads/nope $Z\n" --stdin
    ur_in "delete refs/heads/a $A\n" --stdin
    ur_in "delete refs/heads/a $B\n" --stdin
    ur_in "delete refs/heads/nope\n" --stdin
    ur_in "delete refs/heads/nope $B\n" --stdin
    ur_in "update refs/heads/c $A\ncreate refs/heads/d $B\n" --stdin
    ur_in "update refs/heads/c $B\nupdate refs/heads/d $A $A\n" --stdin
    ur_in "update refs/heads/c $B\nupdate refs/heads/c $A\n" --stdin
    ur_in "update refs/heads/c $B\nverify refs/heads/d $A\n" --stdin
    ur_in "update refs/heads/c $B\ndelete refs/heads/d $B\n" --stdin
    ur_in "update refs/heads/c $Z\n" --stdin
    ur_in "update refs/heads/c $B\n" --stdin
    ur_in "update refs/heads/c '' $B\n" --stdin
    ur_in "update refs/heads/c $A\n" -m viastdin --stdin
    ur_in "option no-deref\nupdate HEAD $A\n" --stdin
    ur_in "option bogus\nupdate refs/heads/c $A\n" --stdin
    ur_in "update refs/heads/c\n" --stdin
    ur_in "update refs/heads/c nothex\n" --stdin
    ur_in "update refs/heads/c $B $A extra\n" --stdin
    ur_in "update refs/heads/c $B nothex\n" --stdin
    ur_in "update bad..name $B\n" --stdin
    ur_in "bogus x\n" --stdin
    ur_in "update\n" --stdin
    ur_in "" --stdin
    ur_in "update refs/heads/c $B\n" --stdin extra
    # transactions
    ur_in "start\nupdate refs/heads/c $A\nprepare\ncommit\n" --stdin
    ur_in "start\nupdate refs/heads/c $B\nprepare\nabort\n" --stdin
    ur_in "start\nupdate refs/heads/c $B\nprepare\n" --stdin
    ur_in "start\nupdate refs/heads/c $B\n" --stdin
    ur_in "start\nupdate refs/heads/c $B\ncommit\n" --stdin
    ur_in "start\nupdate refs/heads/c $A $B\nupdate refs/heads/e $B\nprepare\ncommit\n" --stdin
    ur_in "start\nupdate refs/heads/c $A $B\nprepare\ncommit\n" --stdin
    ur_in "start\nstart\n" --stdin
    ur_in "prepare\n" --stdin
    ur_in "commit\n" --stdin
    ur_in "abort\n" --stdin
    ur_in "start\nabort\ncommit\n" --stdin
    ur_in "start\ncommit\nupdate refs/heads/c $B\n" --stdin
    v for-each-ref
    # --stdin -z
    ur_in "update refs/heads/z\x00$A\x00\x00" -z --stdin
    ur_in "update refs/heads/z\x00$B\x00$A\x00" -z --stdin
    ur_in "update refs/heads/z\x00$B\x00$A\x00" -z --stdin
    ur_in "update refs/heads/z\x00$A\x00" -z --stdin
    ur_in "create refs/heads/z2\x00$A\x00" -z --stdin
    ur_in "create refs/heads/z2\x00$A\x00" -z --stdin
    ur_in "verify refs/heads/z2\x00$A\x00" -z --stdin
    ur_in "verify refs/heads/z2\x00$B\x00" -z --stdin
    ur_in "delete refs/heads/z2\x00$B\x00" -z --stdin
    ur_in "delete refs/heads/z2\x00\x00" -z --stdin
    ur_in "start\x00update refs/heads/z\x00$A\x00\x00prepare\x00abort\x00" -z --stdin
    ur_in "start\x00update refs/heads/z\x00$A\x00\x00commit\x00" -z --stdin
    ur_in "option no-deref\x00update HEAD\x00$A\x00\x00" -z --stdin
    v for-each-ref
    v reflog show refs/heads/c
    v reflog show HEAD
    v fsck
}

s_commit_tree() {
    v init -q repo; cd repo
    echo a >a; v add a; tick; v commit -q -m one
    echo b >a; v commit -q -a -m two
    T=$(g rev-parse HEAD^{tree}); P=$(g rev-parse HEAD); Q=$(g rev-parse HEAD~1)
    B=$(g rev-parse HEAD:a)
    printf 'from a file\n\n\n' >msg.txt
    printf 'no newline' >raw.txt
    # messages: one -m, several, -F, mixed, stdin, empty, no trailing newline
    v commit-tree $T -m one
    v commit-tree $T -m one -m two -m 'three
'
    v commit-tree $T -m 'multi
line' -m ''
    v commit-tree $T -F msg.txt
    v commit-tree $T -F msg.txt -m after
    v commit-tree $T -m before -F raw.txt -F msg.txt
    v commit-tree $T -F raw.txt
    vin 'from stdin
' commit-tree $T
    vin 'a
b' commit-tree $T
    vin 'dash
' commit-tree $T -F -
    vin 'also stdin
' commit-tree $T -m ''
    v commit-tree $T
    v commit-tree -m x $T
    v commit-tree -mx -p$P $T
    v commit-tree $T -m x --
    v commit-tree -- $T -m x
    # parents
    v commit-tree $T -p $P -m child
    v commit-tree $T -p $P -p $Q -m merge
    v commit-tree $T -p $Q -p $P -m merge
    v commit-tree $T -p HEAD -p HEAD~1 -m names
    v commit-tree $T -p $P -p $P -m dup
    v commit-tree $T -p $T -m nottree
    v commit-tree $T -p nosuch -m x
    v commit-tree $T -p ${P%?}0 -m x
    # trees
    v commit-tree HEAD^{tree} -m peeled
    v commit-tree $P -m commitastree
    v commit-tree $B -m blobastree
    v commit-tree nosuch -m x
    v commit-tree
    v commit-tree $T $T -m x
    v commit-tree $T -m x extra
    # usage and file errors
    v commit-tree $T -p
    v commit-tree $T -m
    v commit-tree $T -F
    v commit-tree $T -F nofile.txt
    v commit-tree $T --bogus
    v commit-tree $T -Z
    # identity and dates
    GIT_AUTHOR_DATE='2020-01-02T03:04:05+0200' GIT_COMMITTER_DATE='@1600000000 -0500' v commit-tree $T -m dates
    GIT_AUTHOR_DATE='1234567890 +0000' v commit-tree $T -m authordate
    GIT_COMMITTER_DATE='2021-06-07 08:09:10 +0530' v commit-tree $T -m committerdate
    GIT_AUTHOR_DATE=garbage v commit-tree $T -m x
    GIT_COMMITTER_DATE=garbage v commit-tree $T -m x
    GIT_AUTHOR_NAME='Other Person' GIT_AUTHOR_EMAIL='o@example.com' GIT_COMMITTER_NAME='Third' GIT_COMMITTER_EMAIL='t@example.com' v commit-tree $T -m ident
    v config user.name 'Config Name'
    v config user.email 'config@example.com'
    v commit-tree $T -m config
    # the object must be a readable commit and nothing moved
    O=$(g commit-tree $T -p $P -m final)
    v cat-file -p $O
    v cat-file -t $O
    v fsck
    v for-each-ref
    v rev-parse HEAD
    # bytes that are not UTF-8: the same warning, the same bytes
    printf 'bad \xff\xfe bytes\n' >bad.txt
    v commit-tree $T -F bad.txt
    printf 'nonchar \xef\xbf\xbe and trunc \xe2\x82\n' >bad2.txt
    v commit-tree $T -F bad2.txt
    v config i18n.commitEncoding utf-8
    v commit-tree $T -m 'utf8 spelled out'
}

s_diff_tree() {
    v init -q repo; cd repo
    mkdir d e; echo 1 >a; echo 2 >d/b; echo 3 >d/c; echo 4 >e/f
    ln -s a link
    v add .; tick; v commit -q -m one
    ONE=$(g rev-parse HEAD)
    echo 4 >>a; echo 5 >d/b; v rm -q d/c; echo n >d/n; chmod +x a
    v rm -q -r e; mkdir e; echo x >e; v add -A; tick; v commit -q -m two
    git_branch=$(g rev-parse --abbrev-ref HEAD)
    v checkout -q -b side HEAD~1
    echo s >s; v add s; tick; v commit -q -m side
    v merge -q --no-edit "$git_branch"
    for o in "" "-r" "-t -r" "-r --root" "--root" "-r --no-commit-id" "-r --name-only" "-r --name-status" \
        "--name-only" "--name-status" "-r --stat" "--stat" "-r -p" "-p" "-r -z" "-r --name-only -z" \
        "-r --name-status -z" "-r --abbrev" "-r --abbrev=10" "-s" "-r -s" "-r -m" "-r --first-parent" \
        "-r -m --name-status" "-r -m --stat" "-r -m -p" "-r --name-only --stat" "-r --stat -p"; do
        for rev in HEAD HEAD~1 HEAD^2 HEAD~2 "$ONE"; do
            v diff-tree $o $rev
        done
    done
    v diff-tree -r HEAD~1 HEAD
    v diff-tree -r HEAD HEAD~1
    v diff-tree -r HEAD~2 HEAD
    v diff-tree HEAD~2 HEAD
    v diff-tree -t -r HEAD~2 HEAD
    v diff-tree -r HEAD~2 HEAD -- d
    v diff-tree HEAD~2 HEAD -- d/b
    v diff-tree -r HEAD~2 HEAD -- d/b a
    v diff-tree -r HEAD~2 HEAD -- nosuchpath
    v diff-tree -r --stat HEAD~2 HEAD -- d
    v diff-tree -r -p HEAD~2 HEAD -- d/b
    v diff-tree -r --root HEAD~2 -- d
    v diff-tree -r 'HEAD^{tree}' HEAD~2
    v diff-tree -r 'HEAD^{tree}'
    v diff-tree -r HEAD~2:d HEAD:d
    v diff-tree -r HEAD HEAD
    v diff-tree -r nosuch
    v diff-tree -r HEAD nosuch
    v diff-tree -r HEAD:a
    v diff-tree
    v diff-tree --bogus HEAD
    # a symlink replaced by a regular file is a type change in the raw formats
    rm link; echo notlink >link; v add -A; tick; v commit -q -m retype
    for o in "-r" "-r --name-status" "-r --name-only" "-t -r"; do
        v diff-tree $o HEAD
    done
}

case "${1:-}" in
    -h|--help)
        declare -F | awk '{print $3}' | grep '^s_'
        exit 0 ;;
esac

if [ $# -eq 0 ]; then
    set -- $(declare -F | awk '{print $3}' | grep '^s_')
fi
for s in "$@"; do
    case $s in
        s_remote_fetch_push|s_ssh|s_http|s_pull|s_push_cases|s_pull_merge|s_fetch_prune_tags|s_packed_refs_stale|s_pull_rebase|s_pull_rebase_fork_point|s_pull_autostash|s_pull_rebase_conflict|s_pull_squash|s_push_refspec_forms) run_scenario "$s" repo origin.git ;;
        s_credential) run_scenario "$s" c_ok c5 ;;
        s_credential_post) run_scenario "$s" c_post ;;
        s_credential_keys) run_scenario "$s" c_exp c_quitfull ;;
        s_credential_authtype) run_scenario "$s" c_bearer c_bare c_first ;;
        s_credential_state) run_scenario "$s" c_both c_stateonly c_nocap ;;
        s_credential_url) run_scenario "$s" c_same c_moved c_cleared c_junk c_nl ;;
        s_hooks_push) run_scenario "$s" repo origin.git other.git ;;
        s_sparse_linked) run_scenario "$s" repo wt ;;
        s_rev_parse_dirs) run_scenario "$s" repo wt bare.git ;;
        s_hooks_clone) run_scenario "$s" c1 c2 c4 bare.git wt1 wt2 wt3 wt4 wt5 ;;
        s_hooks_rebase) run_scenario "$s" repo origin.git ;;
        s_pack_deltas) run_scenario "$s" origin cl cl2 ;;
        s_worktree_linked) run_scenario "$s" repo wt wd ;;
        s_worktree_branch_guards) run_scenario "$s" repo o d ;;
        s_worktree_force_guards) run_scenario "$s" repo o ;;
        s_worktree_gc_keeps) run_scenario "$s" repo d e ;;
        s_submodule_status) run_scenario "$s" top ;;
        s_worktree_basic) run_scenario "$s" repo w1 w2 w3 ;;
        s_worktree_lock_remove) run_scenario "$s" repo e g ;;
        s_submodule_init) run_scenario "$s" clone clone2 clone3 clone4 ;;
        s_submodule_sync) run_scenario "$s" clone ;;
        s_submodule_add) run_scenario "$s" top top/vendor/lib top/dev/lib2 ;;
        s_submodule_add_urls) run_scenario "$s" none ;;
        s_submodule_update) run_scenario "$s" clone clone/vendor/lib ;;
        s_submodule_update_recursive) run_scenario "$s" clone ;;
        s_submodule_update_single_branch) run_scenario "$s" clone ;;
        s_submodule_update_reference) run_scenario "$s" clone ;;
        s_submodule_update_remote) run_scenario "$s" clone ;;
        s_submodule_update_merge) run_scenario "$s" clone ;;
        s_submodule_update_rebase) run_scenario "$s" clone ;;
        s_submodule_update_depth) run_scenario "$s" clone ;;
        s_submodule_deinit) run_scenario "$s" clone clone2 ;;
        s_submodule_foreach) run_scenario "$s" none ;;
        s_bundle_roundtrip) run_scenario "$s" empty e2 e3 e4 ;;
        s_ls_remote) run_scenario "$s" none ;;
        s_ls_remote_patterns) run_scenario "$s" none ;;
        s_ls_remote_sort) run_scenario "$s" none ;;
        s_ls_remote_version_sort) run_scenario "$s" none ;;
        s_ls_remote_version_suffix) run_scenario "$s" none ;;
        s_clone_filter) run_scenario "$s" local viafile bare.git limit ;;
        s_clone_bundle_uri) run_scenario "$s" seeded viafile bare.git missing remote ;;
        s_bundle_clone) run_scenario "$s" all named all.git onside upstream nohead dotted odd.git ;;
        s_submodule_set) run_scenario "$s" clone ;;
        s_submodule_absorb) run_scenario "$s" top top/vendor/lib ;;
        s_submodule_summary) run_scenario "$s" none ;;
        s_worktree_move) run_scenario "$s" repo ;;
        s_worktree_repair) run_scenario "$s" repo ;;
        s_worktree_repair_main) run_scenario "$s" moved2 ;;
        s_clone) run_scenario "$s" repo bare.git sidecl origin.git ;;
        s_attributes_eol) run_scenario "$s" repo plain ;;
        s_alternates) run_scenario "$s" repo cl cl2 rel/r ;;
        s_shallow) run_scenario "$s" src cl ;;
        *) run_scenario "$s" ;;
    esac
done
echo "failures: $FAILURES"
exit $((FAILURES > 0))
