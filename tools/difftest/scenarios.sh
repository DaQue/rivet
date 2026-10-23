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

if [ $# -eq 0 ]; then
    set -- $(declare -F | awk '{print $3}' | grep '^s_')
fi
for s in "$@"; do
    case $s in
        s_remote_fetch_push|s_ssh|s_http|s_pull|s_push_cases|s_pull_merge|s_fetch_prune_tags|s_pull_rebase|s_pull_rebase_fork_point|s_pull_autostash|s_pull_rebase_conflict|s_pull_squash) run_scenario "$s" repo origin.git ;;
        s_pack_deltas) run_scenario "$s" origin cl cl2 ;;
        s_clone) run_scenario "$s" repo bare.git sidecl origin.git ;;
        *) run_scenario "$s" ;;
    esac
done
echo "failures: $FAILURES"
exit $((FAILURES > 0))
