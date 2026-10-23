//! The ASCII history graph of `log --graph`, ported from Git's `graph.c`.
//!
//! The graph is a state machine fed one commit at a time with [`Graph::update`]. Between commits it
//! hands out one text line per call (`next_line`): optional "pre-commit" lines that widen the
//! space around an octopus merge, the commit line itself (the one holding the `*`), a "post-merge"
//! line with the `|\` connectors, and "collapsing" lines that pull columns back to the left.
//! Everything before a line's graph prefix is the caller's business; this module only writes
//! the prefix and pads it to the graph's current width.

use rivet_core::ObjectId;

/// The colours Git cycles through for columns (`column_colors` in `graph.c`), without the reset.
const COLUMN_COLORS: [&str; 12] = [
    "\x1b[31m",
    "\x1b[32m",
    "\x1b[33m",
    "\x1b[34m",
    "\x1b[35m",
    "\x1b[36m",
    "\x1b[1;31m",
    "\x1b[1;32m",
    "\x1b[1;33m",
    "\x1b[1;34m",
    "\x1b[1;35m",
    "\x1b[1;36m",
];
const RESET: &str = "\x1b[m";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Padding,
    Skip,
    PreCommit,
    Commit,
    PostMerge,
    Collapsing,
    /// The connector line leading a shifted root commit into its column.
    Shift,
}

#[derive(Clone, Copy, Debug)]
struct Column {
    commit: ObjectId,
    color: usize,
}

/// One output line under construction: bytes plus the number of visible columns written.
struct Line {
    bytes: Vec<u8>,
    width: usize,
}

impl Line {
    fn new() -> Self {
        Line {
            bytes: Vec::new(),
            width: 0,
        }
    }

    fn addch(&mut self, c: char) {
        self.bytes.push(c as u8);
        self.width += 1;
    }

    fn addchars(&mut self, c: char, n: usize) {
        for _ in 0..n {
            self.addch(c);
        }
    }

    fn write_column(&mut self, column: &Column, c: char, colored: bool) {
        if colored {
            self.bytes
                .extend_from_slice(COLUMN_COLORS[column.color].as_bytes());
        }
        self.addch(c);
        if colored {
            self.bytes.extend_from_slice(RESET.as_bytes());
        }
    }
}

/// The layout state of `log --graph` between two commits.
pub struct Graph {
    colored: bool,
    first_parent: bool,
    commit: Option<ObjectId>,
    parents: Vec<ObjectId>,
    num_parents: usize,
    width: usize,
    expansion_row: usize,
    state: State,
    prev_state: State,
    commit_index: usize,
    prev_commit_index: usize,
    /// How many columns the current root commit is drawn to the right of its own column.
    shift: usize,
    /// Whether the current commit already had a column (a child above it) before this row.
    commit_in_columns: bool,
    merge_layout: i32,
    /// Screen column where a merge's left parent edge starts (`mapping_idx` for layout 0).
    left_edge: i32,
    edges_added: i32,
    prev_edges_added: i32,
    columns: Vec<Column>,
    new_columns: Vec<Column>,
    mapping: Vec<i32>,
    old_mapping: Vec<i32>,
    default_column_color: usize,
}

impl Graph {
    /// An empty graph. `colored` paints each column with Git's column colours.
    pub fn new(colored: bool, first_parent: bool) -> Self {
        Graph {
            colored,
            first_parent,
            commit: None,
            parents: Vec::new(),
            num_parents: 0,
            width: 0,
            expansion_row: 0,
            state: State::Padding,
            prev_state: State::Padding,
            commit_index: 0,
            prev_commit_index: 0,
            shift: 0,
            commit_in_columns: false,
            merge_layout: 0,
            left_edge: 0,
            edges_added: 0,
            prev_edges_added: 0,
            columns: Vec::new(),
            new_columns: Vec::new(),
            mapping: Vec::new(),
            old_mapping: Vec::new(),
            default_column_color: COLUMN_COLORS.len() - 1,
        }
    }

    /// For the commit passed to the last [`Graph::update`]: the column it sits in with no shift,
    /// how many columns carry on below it, and whether a child above it already had a column for it.
    pub fn row_layout(&self) -> (usize, usize, bool) {
        (
            self.commit_index,
            self.new_columns.len(),
            self.commit_in_columns,
        )
    }

    /// Move to the next commit to be shown, whose parents are `parents`. A root commit is drawn
    /// `shift` columns to the right of the column it would normally take.
    pub fn update(&mut self, commit: ObjectId, parents: &[ObjectId], shift: usize) {
        self.commit = Some(commit);
        self.shift = shift;
        self.parents = if self.first_parent {
            parents.iter().take(1).copied().collect()
        } else {
            parents.to_vec()
        };
        self.num_parents = self.parents.len();
        self.prev_commit_index = self.commit_index;
        self.update_columns();
        self.expansion_row = 0;
        if self.state != State::Padding {
            self.state = State::Skip;
        } else if self.needs_pre_commit_line() {
            self.state = State::PreCommit;
        } else {
            self.state = self.state_before_commit();
        }
    }

    /// The state that precedes the commit line: a shifted root that had a column first needs a
    /// connector line leading into its new column.
    fn state_before_commit(&self) -> State {
        if self.shift > 0 && self.commit_in_columns {
            State::Shift
        } else {
            State::Commit
        }
    }

    /// Whether every line belonging to the current commit has been produced.
    pub fn is_commit_finished(&self) -> bool {
        self.state == State::Padding
    }

    /// Write the lines up to and including the commit line. Every line but the commit line is
    /// newline-terminated; the commit line is left open for the commit's text to follow.
    pub fn show_commit(&mut self, out: &mut Vec<u8>) {
        if self.commit.is_none() {
            return;
        }
        loop {
            if self.is_commit_finished() {
                self.padding_line(out);
                return;
            }
            let shown_commit_line = self.next_line(out);
            if shown_commit_line {
                return;
            }
            out.push(b'\n');
        }
    }

    /// Write whatever lines the current commit still needs, each newline-terminated.
    pub fn show_remainder(&mut self, out: &mut Vec<u8>) {
        while !self.is_commit_finished() {
            self.next_line(out);
            out.push(b'\n');
        }
    }

    /// A line that leaves every branch line as it is. While the commit line is still to come this
    /// does not advance the state machine; in every other state it is the next line.
    pub fn padding_line(&mut self, out: &mut Vec<u8>) {
        if self.state != State::Commit {
            self.next_line(out);
            return;
        }
        // Observed against git: a padding row drawn ahead of the commit line (the blank separator
        // between entries) hides how the previous commit ended from the commit line's checks.
        self.prev_state = State::Commit;
        let mut line = Line::new();
        for column in &self.columns {
            line.write_column(column, '|', self.colored);
            if Some(column.commit) == self.commit && self.num_parents > 2 {
                line.addchars(' ', (self.num_parents - 2) * 2);
            } else {
                line.addch(' ');
            }
        }
        // The room a shifted root will take is not claimed until its own row.
        let width = self.width - 2 * self.shift;
        if line.width < width {
            line.addchars(' ', width - line.width);
        }
        out.extend_from_slice(&line.bytes);
    }

    /// Produce the next graph line without a newline. `true` when it is the commit line.
    pub fn next_line(&mut self, out: &mut Vec<u8>) -> bool {
        let mut line = Line::new();
        let shown_commit_line = match self.state {
            State::Padding => {
                self.output_padding_line(&mut line);
                false
            }
            State::Skip => {
                self.output_skip_line(&mut line);
                false
            }
            State::PreCommit => {
                self.output_pre_commit_line(&mut line);
                false
            }
            State::Commit => {
                self.output_commit_line(&mut line);
                true
            }
            State::PostMerge => {
                self.output_post_merge_line(&mut line);
                false
            }
            State::Collapsing => {
                self.output_collapsing_line(&mut line);
                false
            }
            State::Shift => {
                self.output_shift_line(&mut line);
                false
            }
        };
        out.extend_from_slice(&line.bytes);
        shown_commit_line
    }

    fn set_state(&mut self, state: State) {
        self.prev_state = self.state;
        self.state = state;
    }

    fn pad_horizontally(&self, line: &mut Line) {
        if line.width < self.width {
            line.addchars(' ', self.width - line.width);
        }
    }

    fn current_column_color(&self) -> usize {
        self.default_column_color
    }

    fn increment_column_color(&mut self) {
        self.default_column_color = (self.default_column_color + 1) % COLUMN_COLORS.len();
    }

    fn find_commit_color(&self, commit: &ObjectId) -> usize {
        self.columns
            .iter()
            .find(|c| c.commit == *commit)
            .map_or_else(|| self.current_column_color(), |c| c.color)
    }

    fn num_dashed_parents(&self) -> usize {
        (self.num_parents as i32 + self.merge_layout - 3).max(0) as usize
    }

    fn num_expansion_rows(&self) -> usize {
        self.num_dashed_parents() * 2
    }

    fn needs_pre_commit_line(&self) -> bool {
        self.num_parents >= 3
            && self.commit_index + 1 < self.columns.len()
            && self.expansion_row < self.num_expansion_rows()
    }

    fn find_new_column_by_commit(&self, commit: &ObjectId) -> Option<usize> {
        self.new_columns.iter().position(|c| c.commit == *commit)
    }

    fn insert_into_new_columns(&mut self, commit: ObjectId, idx: i32) {
        let i = match self.find_new_column_by_commit(&commit) {
            Some(i) => i,
            None => {
                let color = self.find_commit_color(&commit);
                self.new_columns.push(Column { commit, color });
                self.new_columns.len() - 1
            }
        };
        let mapping_idx;
        if self.num_parents > 1 && idx > -1 && self.merge_layout == -1 {
            // The first parent of a merge: choose a layout for the merge line by whether the
            // parent sits in a column to the left of the merge.
            let dist = idx - i as i32;
            let shift = if dist > 1 { 2 * dist - 3 } else { 1 };
            self.merge_layout = if dist > 0 { 0 } else { 1 };
            self.edges_added = self.num_parents as i32 + self.merge_layout - 2;
            mapping_idx = self.width as i32 + (self.merge_layout - 1) * shift;
            self.left_edge = mapping_idx;
            self.width += 2 * self.merge_layout as usize;
        } else if self.edges_added > 0
            && self.width >= 2
            && self.mapping[self.width - 2] == i as i32
        {
            // A merge added columns, but this commit was already in `columns` at that position.
            mapping_idx = self.width as i32 - 2;
            self.edges_added = -1;
        } else {
            mapping_idx = self.width as i32;
            self.width += 2;
        }
        self.mapping[mapping_idx as usize] = i as i32;
    }

    fn update_columns(&mut self) {
        // `columns` becomes the state for this commit; `new_columns` is rebuilt for the next one.
        std::mem::swap(&mut self.columns, &mut self.new_columns);
        self.new_columns.clear();
        let max_new_columns = self.columns.len() + self.num_parents;
        self.old_mapping = std::mem::take(&mut self.mapping);
        self.mapping = vec![-1; 2 * max_new_columns + 2];
        let mut mapping_size = 2 * max_new_columns;
        self.width = 0;
        self.prev_edges_added = self.edges_added;
        self.edges_added = 0;

        let commit = self.commit.expect("update sets the commit");
        let mut seen_this = false;
        let mut is_commit_in_columns = true;
        self.commit_in_columns = self.columns.iter().any(|c| c.commit == commit);
        let num_columns = self.columns.len();
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                is_commit_in_columns = false;
                commit
            } else {
                self.columns[i].commit
            };
            if col_commit == commit {
                seen_this = true;
                self.commit_index = i;
                self.merge_layout = -1;
                for parent in self.parents.clone() {
                    // A merge, or the start of a new childless column, takes the next colour.
                    if self.num_parents > 1 || !is_commit_in_columns {
                        self.increment_column_color();
                    }
                    self.insert_into_new_columns(parent, i as i32);
                }
                // The commit takes at least two columns even with no parents.
                if self.num_parents == 0 {
                    self.width += 2;
                }
            } else {
                self.insert_into_new_columns(col_commit, -1);
            }
        }
        while mapping_size > 1 && self.mapping[mapping_size - 1] < 0 {
            mapping_size -= 1;
        }
        self.mapping.truncate(mapping_size);
        // The room a shifted root takes is padding only; the columns below it map as if it had not moved.
        if self.num_parents == 0 {
            self.width += 2 * self.shift;
        }
    }

    fn is_mapping_correct(&self) -> bool {
        self.mapping
            .iter()
            .enumerate()
            .all(|(i, &target)| target < 0 || target as usize == i / 2)
    }

    fn output_padding_line(&self, line: &mut Line) {
        for column in &self.new_columns {
            line.write_column(column, '|', self.colored);
            line.addch(' ');
        }
        self.pad_horizontally(line);
    }

    fn output_skip_line(&mut self, line: &mut Line) {
        for c in "...".chars() {
            line.addch(c);
        }
        self.pad_horizontally(line);
        if self.num_parents >= 3 && self.commit_index + 1 < self.columns.len() {
            self.set_state(State::PreCommit);
        } else {
            let next = self.state_before_commit();
            self.set_state(next);
        }
    }

    fn output_shift_line(&mut self, line: &mut Line) {
        let commit = self.commit.expect("a commit is being shown");
        for column in &self.columns {
            if column.commit == commit {
                line.addch(' ');
                line.write_column(column, '\\', self.colored);
                line.addch(' ');
            } else {
                line.write_column(column, '|', self.colored);
                line.addch(' ');
            }
        }
        self.set_state(State::Commit);
    }

    fn output_pre_commit_line(&mut self, line: &mut Line) {
        let mut seen_this = false;
        for (i, column) in self.columns.iter().enumerate() {
            if Some(column.commit) == self.commit {
                seen_this = true;
                line.write_column(column, '|', self.colored);
                line.addchars(' ', self.expansion_row);
            } else if seen_this && self.expansion_row == 0 {
                // The first line of pre-commit output continues a `\` from a post-merge line.
                if self.prev_state == State::PostMerge && self.prev_commit_index < i {
                    line.write_column(column, '\\', self.colored);
                } else {
                    line.write_column(column, '|', self.colored);
                }
            } else if seen_this {
                line.write_column(column, '\\', self.colored);
            } else {
                line.write_column(column, '|', self.colored);
            }
            line.addch(' ');
        }
        self.pad_horizontally(line);
        self.expansion_row += 1;
        if !self.needs_pre_commit_line() {
            self.set_state(State::Commit);
        }
    }

    fn draw_octopus_merge(&self, line: &mut Line) {
        // `*---.` : the edges labelled 0 and 1 need no dashes, the rest do.
        let dashless_parents = 3 - self.merge_layout;
        let dashful_parents = self.num_parents as i32 - dashless_parents;
        let parent_in_old_cols = self.num_parents as i32 - self.edges_added;
        let first_col = self.commit_index as i32 + dashless_parents - parent_in_old_cols;
        for i in 0..dashful_parents {
            let column = self.new_columns[(i + first_col) as usize];
            line.write_column(&column, '-', self.colored);
            line.write_column(
                &column,
                if i == dashful_parents - 1 { '.' } else { '-' },
                self.colored,
            );
        }
    }

    fn output_commit_line(&mut self, line: &mut Line) {
        let commit = self.commit.expect("a commit is being shown");
        let mut seen_this = false;
        let num_columns = self.columns.len();
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                commit
            } else {
                self.columns[i].commit
            };
            if col_commit == commit {
                seen_this = true;
                line.addchars(' ', 2 * self.shift);
                line.addch('*');
                if self.num_parents > 2 {
                    self.draw_octopus_merge(line);
                }
            } else if seen_this && self.edges_added > 1 {
                line.write_column(&self.columns[i], '\\', self.colored);
            } else if seen_this && self.edges_added == 1 {
                // A right-skewed 2-way merge or a left-skewed 3-way merge has no pre-commit
                // stage, so this is its first line; a `\` from the previous line carries on.
                if self.prev_state == State::PostMerge
                    && self.prev_edges_added > 0
                    && self.prev_commit_index < i
                {
                    line.write_column(&self.columns[i], '\\', self.colored);
                } else {
                    line.write_column(&self.columns[i], '|', self.colored);
                }
            } else if self.prev_state == State::Collapsing
                && self.old_mapping.get(2 * i + 1) == Some(&(i as i32))
                && self.mapping.get(2 * i).is_some_and(|&t| t < i as i32)
            {
                // The branch the previous row drew sloping into this column keeps sloping.
                line.write_column(&self.columns[i], '/', self.colored);
            } else {
                line.write_column(&self.columns[i], '|', self.colored);
            }
            line.addch(' ');
        }
        self.pad_horizontally(line);
        // Not `set_state`: this line does not count as the "previous" state for the next one.
        self.state = if self.num_parents > 1 {
            State::PostMerge
        } else if self.is_mapping_correct() {
            State::Padding
        } else {
            State::Collapsing
        };
    }

    fn output_post_merge_line(&mut self, line: &mut Line) {
        const MERGE_CHARS: [char; 3] = ['/', '|', '\\'];
        let commit = self.commit.expect("a commit is being shown");
        let mut seen_this = false;
        let num_columns = self.columns.len();
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                commit
            } else {
                self.columns[i].commit
            };
            if col_commit == commit {
                // The edges of a merge are drawn over the columns of its parents.
                seen_this = true;
                let mut idx = self.merge_layout;
                for (n, parent) in self.parents.iter().enumerate() {
                    let par_column = self
                        .find_new_column_by_commit(parent)
                        .expect("a parent has a column");
                    let c = MERGE_CHARS[idx as usize];
                    line.write_column(&self.new_columns[par_column], c, self.colored);
                    if idx == 2 {
                        if self.edges_added > 0 || n + 1 < self.parents.len() {
                            line.addch(' ');
                        }
                    } else {
                        idx += 1;
                    }
                }
                if self.edges_added == 0 {
                    line.addch(' ');
                }
            } else if seen_this {
                let c = if self.edges_added > 0 { '\\' } else { '|' };
                line.write_column(&self.columns[i], c, self.colored);
                line.addch(' ');
            } else {
                line.write_column(&self.columns[i], '|', self.colored);
                if self.merge_layout != 0 || i + 1 != self.commit_index {
                    // A left parent far away is reached by a run of `_` from where its edge starts.
                    if self.merge_layout == 0 && (2 * i + 1) as i32 >= self.left_edge {
                        let left_parent = self
                            .parents
                            .first()
                            .and_then(|p| self.find_new_column_by_commit(p))
                            .expect("a parent has a column");
                        line.write_column(&self.new_columns[left_parent], '_', self.colored);
                    } else {
                        line.addch(' ');
                    }
                }
            }
        }
        self.pad_horizontally(line);
        if self.is_mapping_correct() {
            self.set_state(State::Padding);
        } else {
            self.set_state(State::Collapsing);
        }
    }

    fn output_collapsing_line(&mut self, line: &mut Line) {
        let size = self.mapping.len();
        let mut used_horizontal = false;
        let mut horizontal_edge: i32 = -1;
        let mut horizontal_edge_target: i32 = -1;
        let mut new_mapping = vec![-1i32; size + 2];

        for i in 0..size {
            let target = self.mapping[i];
            if target < 0 {
                continue;
            }
            // Branches only ever move left, so `target * 2 <= i`.
            if target as usize * 2 == i {
                new_mapping[i] = target;
            } else if i >= 1 && new_mapping[i - 1] < 0 {
                // Nothing to the left: move left by one.
                new_mapping[i - 1] = target;
                if horizontal_edge == -1 {
                    horizontal_edge = i as i32;
                    horizontal_edge_target = target;
                    let mut j = target as usize * 2 + 3;
                    while j + 2 < i {
                        new_mapping[j] = target;
                        j += 2;
                    }
                }
            } else if i >= 1 && new_mapping[i - 1] == target {
                // A branch line to the left already leads to the same parent: merge into it.
            } else {
                // A branch line to the left leads elsewhere: cross over it.
                new_mapping[i - 2] = target;
                if horizontal_edge == -1 {
                    horizontal_edge = i as i32 - 1;
                    horizontal_edge_target = target;
                    let mut j = target as usize * 2 + 3;
                    while j + 2 < i {
                        new_mapping[j] = target;
                        j += 2;
                    }
                }
            }
        }

        let mut mapping_size = size;
        if new_mapping[mapping_size - 1] < 0 {
            mapping_size -= 1;
        }

        // Indexed on purpose: the body rewrites `new_mapping` entries the next row relies on.
        #[allow(clippy::needless_range_loop)]
        for i in 0..mapping_size {
            let target = new_mapping[i];
            if target < 0 {
                line.addch(' ');
            } else if target as usize * 2 == i {
                line.write_column(&self.new_columns[target as usize], '|', self.colored);
            } else if target == horizontal_edge_target && i as i32 != horizontal_edge - 1 {
                // Only the first segment of a horizontal run continues into the next line.
                if i != target as usize * 2 + 3 {
                    new_mapping[i] = -1;
                }
                used_horizontal = true;
                line.write_column(&self.new_columns[target as usize], '_', self.colored);
            } else {
                if used_horizontal && (i as i32) < horizontal_edge {
                    new_mapping[i] = -1;
                }
                line.write_column(&self.new_columns[target as usize], '/', self.colored);
            }
        }
        self.pad_horizontally(line);

        new_mapping.truncate(mapping_size);
        self.mapping = new_mapping;
        if self.is_mapping_correct() {
            self.set_state(State::Padding);
        }
    }
}
