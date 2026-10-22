#!/usr/bin/env bash
# The differential scenarios. Run all: tools/difftest/scenarios.sh; run some: ... name1 name2
FAKESSH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/fakessh"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
NOW0=$(date +%s)   # one wall-clock reading shared by both implementations

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
    # Git keeps young unreachable objects in a cruft pack; Rivet leaves them loose.
    DROP_OUT='^(size|count|in-pack|packs|prune-packable|size-pack)' v count-objects -v
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
    for idx in .git/objects/pack/*.idx; do
        [ -e "$idx" ] || continue
        g verify-pack -v "$idx" | awk 'NF == 5 && $1 ~ /^[0-9a-f]+$/ { print $1, $2, $3 } NF == 7 && $1 ~ /^[0-9a-f]+$/ { print $1, $2, $3, $6, $7 }'
    done | sort >>"$TRANSCRIPT"
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
    notes_fill 70000
    tick; v notes add -m extra "$(echo extra | g hash-object -w --stdin)"
    v cat-file -p "$(g rev-parse refs/notes/commits^{tree})"
}

if [ $# -eq 0 ]; then
    set -- $(declare -F | awk '{print $3}' | grep '^s_')
fi
for s in "$@"; do
    case $s in
        s_remote_fetch_push|s_ssh|s_http|s_pull|s_push_cases|s_pull_merge|s_fetch_prune_tags) run_scenario "$s" repo origin.git ;;
        s_pack_deltas) run_scenario "$s" origin cl cl2 ;;
        s_clone) run_scenario "$s" repo bare.git sidecl origin.git ;;
        *) run_scenario "$s" ;;
    esac
done
echo "failures: $FAILURES"
exit $((FAILURES > 0))
