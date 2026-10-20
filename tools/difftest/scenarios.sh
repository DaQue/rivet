#!/usr/bin/env bash
# The differential scenarios. Run all: tools/difftest/scenarios.sh; run some: ... name1 name2
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

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

s_fsck_verify_pack() {
    g init -q repo; cd repo
    echo a >a; g add a; g commit -q -m one; tick; echo b >>a; g commit -qam two
    g repack -adq
    v fsck
    v fsck --strict
    v verify-pack .git/objects/pack/*.idx
    v verify-pack -v .git/objects/pack/*.idx
    v verify-pack -s .git/objects/pack/*.idx
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

if [ $# -eq 0 ]; then
    set -- $(declare -F | awk '{print $3}' | grep '^s_')
fi
for s in "$@"; do
    case $s in
        s_remote_fetch_push|s_pull) run_scenario "$s" repo origin.git ;;
        s_clone) run_scenario "$s" repo bare.git sidecl origin.git ;;
        *) run_scenario "$s" ;;
    esac
done
echo "failures: $FAILURES"
exit $((FAILURES > 0))
