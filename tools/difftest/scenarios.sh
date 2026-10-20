#!/usr/bin/env bash
# The differential scenarios. Run all: tools/difftest/scenarios.sh; run some: ... name1 name2
FAKESSH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/fakessh"
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
    seq 1 400 >big; g add big; g commit -q -m one
    tick; seq 1 400 | sed 's/^200$/changed/' >big; echo extra >small; g add .; g commit -qm two
    tick; seq 1 400 | sed 's/^100$/other/;s/^200$/changed/' >big; g commit -qam three
    g repack -adq --depth=10 --window=10
    vs fsck
    vs fsck --strict
    v verify-pack .git/objects/pack/*.idx
    v verify-pack --verbose .git/objects/pack/*.idx
    v verify-pack -s .git/objects/pack/*.idx
    echo orphan | g hash-object -w --stdin >/dev/null
    g commit-tree -m orphan HEAD^{tree} >/dev/null
    vs fsck
    vs fsck --unreachable --no-reflogs
    vs fsck --root
    vs fsck --no-dangling
}

s_fsck_missing() {
    g init -q repo; cd repo
    echo a >a; mkdir d; echo b >d/b; g add .; g commit -qm one
    blob=$(g rev-parse HEAD:d/b); rm .git/objects/${blob:0:2}/${blob:2}
    vs fsck
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

if [ $# -eq 0 ]; then
    set -- $(declare -F | awk '{print $3}' | grep '^s_')
fi
for s in "$@"; do
    case $s in
        s_remote_fetch_push|s_ssh|s_pull|s_push_cases|s_pull_merge|s_fetch_prune_tags) run_scenario "$s" repo origin.git ;;
        s_clone) run_scenario "$s" repo bare.git sidecl origin.git ;;
        *) run_scenario "$s" ;;
    esac
done
echo "failures: $FAILURES"
exit $((FAILURES > 0))
