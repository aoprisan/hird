//! TUI state and key handling, kept free of any drawing so it can be tested
//! without a terminal.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::db::Db;
use crate::glob;
use crate::identity::ACTOR_TUI;
use crate::model::{
    Assertion, Blocker, Conflict, Footprint, Question, Standing, Status, Task, TaskEvent,
    TaskSummary, Verdict,
};
use crate::repo::{dispatch_waves, MemoryQuery, ProjectScope, Recalled};

/// How often the database is re-read.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How often the board looks at the working tree, as opposed to the database.
///
/// Slower than the poll on purpose. A database refresh is a few indexed reads
/// of a file already in the page cache; a witness sweep spawns git and hashes
/// whatever is dirty. Two seconds is still faster than an agent can finish a
/// thought, and it keeps an idle board off the CPU.
pub const WITNESS_INTERVAL: chrono::Duration = chrono::Duration::milliseconds(2_000);

/// Upper bound on rows pulled into the memory browser at once.
const MEMORY_PAGE: usize = 200;

/// The three screens, cycled with `Tab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Queue,
    Memory,
    /// Who is working what, where they overlap, and what is workable next.
    Swarm,
}

impl Screen {
    fn next(self) -> Screen {
        match self {
            Screen::Queue => Screen::Memory,
            Screen::Memory => Screen::Swarm,
            Screen::Swarm => Screen::Queue,
        }
    }

    fn previous(self) -> Screen {
        match self {
            Screen::Queue => Screen::Swarm,
            Screen::Memory => Screen::Queue,
            Screen::Swarm => Screen::Memory,
        }
    }
}

/// One live agent, as the swarm screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub holder: String,
    pub seq: i64,
    pub title: String,
    pub status: Status,
    pub lease_expires_at: Option<String>,
    pub patterns: Vec<String>,
    /// Overlaps with the other rows, already resolved to who and where.
    pub overlaps: Vec<Overlap>,
    /// Files the witness saw move while this agent held the task — what
    /// happened, as against `patterns`, which is what was announced.
    pub changed: Vec<String>,
    /// Files this agent and another both declared and that have since moved,
    /// already written out as sentences.
    pub contentions: Vec<String>,
    /// Dependencies of this agent's task that stopped being `done` while it
    /// was held — sent back, reopened, cancelled — already written out as
    /// sentences. An agent building on ground that moved, seen from outside.
    pub shifted: Vec<String>,
    /// Whether anything has moved under this agent at all. `changed` being
    /// empty is not the same statement — it is also what a project hird
    /// cannot watch looks like.
    pub footprint: Footprint,
}

/// Two live agents in the same files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    pub pattern: String,
    pub other_seq: i64,
    pub other_holder: String,
    pub other_pattern: String,
}

/// What a task is waiting for and who it is in the way of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readiness {
    pub waiting_for: Vec<Blocker>,
    /// The finished dependencies the task builds on, each with its own result
    /// and how far that result can currently be trusted.
    pub ground: Vec<crate::model::Ground>,
    pub blocks: Vec<i64>,
    pub paths: Vec<String>,
    pub conflicts: Vec<Conflict>,
    /// Harnesses this task is barred from, already written out.
    pub recusals: Vec<crate::model::Recusal>,
    /// Verdicts delivered on this task's work, oldest first.
    pub verdicts: Vec<crate::model::VerdictRecord>,
    /// Verdicts this task delivered, when it is a completed review.
    pub delivered: Vec<crate::model::VerdictRecord>,
    /// What the working tree says the task did to it: read-only, or so many
    /// files moved. The other half of `paths`, which is only what was said.
    pub footprint: Footprint,
    /// Questions and answers carried by this task, including its current
    /// unresolved question when it is awaiting input.
    pub questions: Vec<Question>,
}

/// A column of the kanban board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Open,
    Active,
    Done,
    Stopped,
}

impl Column {
    pub const ALL: [Column; 4] = [Column::Open, Column::Active, Column::Done, Column::Stopped];

    pub fn title(self) -> &'static str {
        match self {
            Column::Open => "Open",
            Column::Active => "Claimed / In-progress",
            Column::Done => "Done",
            Column::Stopped => "Failed / Cancelled",
        }
    }

    /// Which statuses land in this column.
    pub fn accepts(self, status: Status) -> bool {
        match self {
            Column::Open => status == Status::Open,
            Column::Active => status.is_active(),
            Column::Done => status == Status::Done,
            Column::Stopped => matches!(status, Status::Failed | Status::Cancelled),
        }
    }

    fn index(self) -> usize {
        Column::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }
}

/// What the UI is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Navigating.
    Normal,
    /// The `?` overlay.
    Help,
    /// Editing the queue's text filter, live.
    Filter,
    /// Editing the memory search box, live.
    Search,
    /// The `a` prompt.
    AddTask {
        title: String,
        body: String,
        /// 0 = title, 1 = body.
        focus: usize,
    },
    /// The `d` prompt on the memory screen.
    Supersede { id: String, replacement: String },
    /// The `A` prompt on a task awaiting input.
    Answer {
        seq: i64,
        question: String,
        answer: String,
    },
    /// A task's full body, history and recorded assertions.
    TaskDetail {
        task: Box<Task>,
        events: Vec<TaskEvent>,
        learned: Vec<Assertion>,
        /// What earlier work elsewhere already knows about this task.
        recalled: Vec<Recalled>,
        readiness: Box<Readiness>,
    },
    /// One assertion with its provenance.
    AssertionDetail { assertion: Box<Assertion> },
}

impl Mode {
    /// Whether this mode is capturing text, so single-letter keys are literal.
    pub fn is_text_entry(&self) -> bool {
        matches!(
            self,
            Mode::Filter
                | Mode::Search
                | Mode::AddTask { .. }
                | Mode::Supersede { .. }
                | Mode::Answer { .. }
        )
    }
}

/// A one-line message shown in the status bar until the next keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub is_error: bool,
}

/// The whole TUI state.
pub struct App {
    pub screen: Screen,
    pub mode: Mode,
    pub toast: Option<Toast>,
    pub should_quit: bool,

    pub db_path: PathBuf,
    pub project: String,
    pub all_projects: bool,
    pub config: Config,

    // Queue screen.
    pub tasks: Vec<TaskSummary>,
    pub counts: BTreeMap<Status, i64>,
    pub filter: String,
    pub column: Column,
    selected: [usize; 4],

    /// Task number to the unfinished tasks it waits for.
    pub unmet: BTreeMap<i64, Vec<i64>>,
    /// Open task number to the question holding it out of dispatch.
    pub questions: BTreeMap<i64, Question>,
    /// Task numbers that are somebody's review, so the board can say so
    /// without opening each card.
    pub reviews: BTreeMap<i64, i64>,
    /// Judged task number to the unfinished review of it, so a done card can
    /// say its `done` is still provisional.
    pub under_review: BTreeMap<i64, i64>,
    /// Task number to the newest verdict delivered on its work, so the board
    /// can tell "done" from "done, and seen to be done by another harness".
    pub verdicts: BTreeMap<i64, Verdict>,
    /// Task number to what the working tree says it did: read-only, or so
    /// many files moved. Absent for every task hird was not watching, which
    /// every reader treats as "nothing to say" rather than as "nothing moved".
    pub footprints: BTreeMap<i64, Footprint>,

    // Swarm screen.
    pub agents: Vec<AgentRow>,
    /// Unfinished tasks grouped by how many rounds of work stand in front of
    /// them; wave 0 is workable now.
    pub waves: Vec<Vec<i64>>,
    pub swarm_selected: usize,

    // Memory screen.
    pub assertions: Vec<Assertion>,
    pub memory_total: i64,
    pub query: String,
    pub include_superseded: bool,
    /// Show only the assertions whose footing has moved — the ones a re-read
    /// would actually pay for.
    pub shaky_only: bool,
    /// Assertion id to how its footing compares with the tree right now.
    /// Empty wherever there is no witness, which is why every reader treats a
    /// missing entry as "nothing to say" rather than as "firm".
    pub standings: BTreeMap<String, Standing>,
    /// The selected assertion's other voices, when it has any. Kept to the
    /// selection because that is the only place they are rendered.
    pub voices: BTreeMap<String, String>,
    pub memory_selected: usize,
    /// Task id to human-facing number, for the "learned on #7" badge.
    pub task_seqs: BTreeMap<String, i64>,

    pub last_poll: DateTime<Utc>,
    /// Set when a dispatch hook is configured, so the expiries this board's
    /// own sweeps collect are announced rather than swallowed.
    herald: Option<crate::herald::Herald>,
    /// Set when this project is somewhere the working tree can be watched.
    witness: Option<crate::witness::Witness>,
    last_look: DateTime<Utc>,
    /// The assertion ids `standings` was last computed for, so a changed page
    /// re-reads immediately and an unchanged one waits for the interval.
    footed_ids: Vec<String>,
    last_footing: DateTime<Utc>,
}

impl App {
    pub fn new(db_path: PathBuf, project: String, config: Config) -> App {
        let all_projects = config.all_projects_by_default;
        let config_witness = config.witness(std::path::Path::new(&project));
        App {
            screen: Screen::Queue,
            mode: Mode::Normal,
            toast: None,
            should_quit: false,
            db_path,
            project,
            all_projects,
            config,
            tasks: Vec::new(),
            counts: BTreeMap::new(),
            filter: String::new(),
            column: Column::Open,
            selected: [0; 4],
            unmet: BTreeMap::new(),
            questions: BTreeMap::new(),
            reviews: BTreeMap::new(),
            under_review: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            footprints: BTreeMap::new(),
            agents: Vec::new(),
            waves: Vec::new(),
            swarm_selected: 0,
            assertions: Vec::new(),
            memory_total: 0,
            query: String::new(),
            include_superseded: false,
            shaky_only: false,
            standings: BTreeMap::new(),
            voices: BTreeMap::new(),
            memory_selected: 0,
            task_seqs: BTreeMap::new(),
            last_poll: DateTime::from_timestamp(0, 0).unwrap_or_default(),
            herald: None,
            witness: config_witness,
            last_look: DateTime::from_timestamp(0, 0).unwrap_or_default(),
            footed_ids: Vec::new(),
            last_footing: DateTime::from_timestamp(0, 0).unwrap_or_default(),
        }
    }

    /// Give the board the configured herald, if there is one.
    ///
    /// Separate from [`App::new`] because a test board announces to nothing
    /// and the real one is the only caller that has a database path worth
    /// handing a hook.
    pub fn with_herald(mut self, herald: Option<crate::herald::Herald>) -> App {
        self.herald = herald;
        self
    }

    /// Look at the working tree, at most every [`WITNESS_INTERVAL`].
    ///
    /// The human is watching, not working, so this confirms nothing on any
    /// agent's behalf. A sweep that fails is a board that shows a little less,
    /// never a board that stops.
    fn look(&mut self, db: &Db) {
        let Some(witness) = &self.witness else {
            return;
        };
        let now = Utc::now();
        if now - self.last_look < WITNESS_INTERVAL {
            return;
        }
        self.last_look = now;
        let _ = crate::witness::sweep(db, witness, &self.project, ACTOR_TUI);
    }

    /// The witness memory may read the tree through, if the configuration
    /// lets it.
    pub fn footing(&self) -> Option<&crate::witness::Witness> {
        self.config.footing(self.witness.as_ref())
    }

    /// How the assertion's footing compares with the tree, if hird knows.
    pub fn standing(&self, assertion: &Assertion) -> Option<&Standing> {
        self.standings.get(&assertion.id)
    }

    /// Re-read the footing under the assertions on screen, at most every
    /// [`WITNESS_INTERVAL`].
    ///
    /// Throttled for the same reason [`App::look`] is, and rather harder: a
    /// standing costs one file read and one SHA-256 per distinct anchored file,
    /// and the poll runs twice a second. It is also recomputed immediately
    /// whenever the visible set changes, so typing in the search box never
    /// shows a badge belonging to a row that has scrolled away.
    fn read_footing(&mut self, db: &Db) {
        let ids: Vec<String> = self.assertions.iter().map(|a| a.id.clone()).collect();
        let now = Utc::now();
        let same_rows = ids == self.footed_ids;
        if same_rows && now - self.last_footing < WITNESS_INTERVAL {
            return;
        }
        self.last_footing = now;
        self.footed_ids = ids;
        self.standings =
            crate::footing::standings(db, self.footing(), &self.project, &self.footed_ids);
    }

    /// Who else has stated the selected assertion, if anyone.
    ///
    /// Only the selection, because only the detail overlay shows it — asking
    /// for every row would be two hundred queries twice a second to render one
    /// line that is usually not on screen.
    pub fn voices_of(&self, assertion: &Assertion) -> Option<&String> {
        self.voices.get(&assertion.id)
    }

    pub fn scope(&self) -> ProjectScope {
        ProjectScope::resolve(&self.project, self.all_projects)
    }

    /// Re-read everything the two screens display.
    ///
    /// Cheap enough to run twice a second: four indexed queries against a WAL
    /// database that is usually idle.
    pub fn refresh(&mut self, db: &Db) -> anyhow::Result<()> {
        // Before the reads, and keeping the outcome: every query below sweeps
        // expired leases anyway, but a sweep reports each lapsed lease to
        // exactly one sweeper, so sweeping inside `list` would collect the
        // expiry and leave nobody to announce it. A board left open would
        // then be the reason a dead agent's task is never handed on.
        crate::herald::sweep_announcing(db, &self.config, self.herald.as_ref());
        let scope = self.scope();
        self.tasks = db.tasks().list(&scope, None)?;
        self.counts = db.tasks().counts(&scope)?;
        self.unmet = db.deps().unmet_map(&scope, self.config.clearance())?;
        self.questions = db.questions().unanswered_map(&scope)?;
        self.reviews = db.recusals().reviews(&scope)?;
        // Inverted from `reviews` and filtered to reviews still in flight:
        // the judged card's `done` is provisional exactly while one is.
        self.under_review = self
            .reviews
            .iter()
            .filter(|(review, _)| {
                self.tasks
                    .iter()
                    .any(|t| t.seq == **review && !t.status.is_terminal())
            })
            .map(|(review, judged)| (*judged, *review))
            .collect();
        self.verdicts = db.verdicts().standing(&scope)?;
        self.footprints = db.witnessed().footprints(&scope)?;
        self.waves = dispatch_waves(&self.tasks, &db.deps().edges(&scope)?);
        self.look(db);
        self.agents = agent_rows(
            &self.tasks,
            &db.scopes().declared(&scope, true)?,
            &db.witnessed().seen(&scope, true)?,
            &self.footprints,
        );
        for agent in &mut self.agents {
            agent.contentions = db
                .witnessed()
                .contention(agent.seq)
                .unwrap_or_default()
                .iter()
                .map(crate::model::Contention::describe)
                .collect();
            agent.shifted = db
                .deps()
                .shifted(agent.seq)
                .unwrap_or_default()
                .iter()
                .map(crate::model::Shifted::describe)
                .collect();
        }
        self.assertions = db.memory().search(
            &MemoryQuery::new(&self.query, scope.clone())
                .limit(MEMORY_PAGE)
                .include_superseded(self.include_superseded),
        )?;
        self.read_footing(db);
        if self.shaky_only {
            let standings = &self.standings;
            self.assertions
                .retain(|a| standings.get(&a.id).is_some_and(Standing::needs_checking));
        }
        self.memory_total = db.memory().count_current(&scope)?;
        self.task_seqs = db.tasks().seq_index()?;
        self.last_poll = Utc::now();
        self.clamp_selection();
        Ok(())
    }

    pub fn due_for_refresh(&self, now: DateTime<Utc>) -> bool {
        (now - self.last_poll).to_std().unwrap_or_default() >= POLL_INTERVAL
    }

    /// How long ago the last poll was, for the status bar.
    pub fn poll_age(&self, now: DateTime<Utc>) -> String {
        crate::fmt::age_phrase(&crate::model::fmt_ts(self.last_poll), now)
    }

    // ------------------------------------------------------------ selections

    /// Tasks in a column, after the text filter.
    pub fn column_tasks(&self, column: Column) -> Vec<&TaskSummary> {
        let needle = self.filter.trim().to_lowercase();
        self.tasks
            .iter()
            .filter(|t| column.accepts(t.status))
            .filter(|t| {
                needle.is_empty()
                    || t.title.to_lowercase().contains(&needle)
                    || t.seq.to_string().contains(&needle)
                    || t.claimed_by
                        .as_deref()
                        .is_some_and(|h| h.to_lowercase().contains(&needle))
            })
            .collect()
    }

    pub fn selected_index(&self, column: Column) -> usize {
        self.selected[column.index()]
    }

    /// The task under the cursor, if the current column has any.
    pub fn selected_task(&self) -> Option<&TaskSummary> {
        let tasks = self.column_tasks(self.column);
        tasks.get(self.selected_index(self.column)).copied()
    }

    pub fn selected_assertion(&self) -> Option<&Assertion> {
        self.assertions.get(self.memory_selected)
    }

    pub fn selected_agent(&self) -> Option<&AgentRow> {
        self.agents.get(self.swarm_selected)
    }

    /// Unfinished tasks task `seq` is waiting for. Empty means claimable.
    pub fn blocked_by(&self, seq: i64) -> &[i64] {
        self.unmet.get(&seq).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Tasks an agent could be handed right now: open, and waiting for nothing.
    pub fn workable(&self) -> Vec<&TaskSummary> {
        self.tasks
            .iter()
            .filter(|t| {
                t.status == Status::Open
                    && self.blocked_by(t.seq).is_empty()
                    && !self.questions.contains_key(&t.seq)
            })
            .collect()
    }

    /// Keep every cursor inside its list after the data changes underneath.
    fn clamp_selection(&mut self) {
        for column in Column::ALL {
            let len = self.column_tasks(column).len();
            let idx = &mut self.selected[column.index()];
            *idx = (*idx).min(len.saturating_sub(1));
        }
        self.memory_selected = self
            .memory_selected
            .min(self.assertions.len().saturating_sub(1));
        self.swarm_selected = self.swarm_selected.min(self.agents.len().saturating_sub(1));
    }

    // ------------------------------------------------------------------ keys

    /// Handle one key press. `db` is used for the actions that mutate state.
    pub fn on_key(&mut self, key: KeyEvent, db: &Db) -> anyhow::Result<()> {
        self.toast = None;

        // Ctrl-C always quits, whatever mode we are in.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        // Taking the mode out by value keeps the borrow checker out of the way
        // when a handler needs `&mut self`.
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Help | Mode::TaskDetail { .. } | Mode::AssertionDetail { .. } => {
                // Any key dismisses an overlay; restore it if the key was not
                // a dismissal so scrolling could be added later without churn.
                if !matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char(_)) {
                    self.mode = mode;
                }
            }
            Mode::Filter => {
                self.mode = Mode::Filter;
                self.edit_filter(key, db)?;
            }
            Mode::Search => {
                self.mode = Mode::Search;
                self.edit_search(key, db)?;
            }
            Mode::AddTask { title, body, focus } => {
                self.edit_add_task(key, title, body, focus, db)?
            }
            Mode::Supersede { id, replacement } => self.edit_supersede(key, id, replacement, db)?,
            Mode::Answer {
                seq,
                question,
                answer,
            } => self.edit_answer(key, seq, question, answer, db)?,
            Mode::Normal => self.normal_key(key, db)?,
        }
        Ok(())
    }

    fn normal_key(&mut self, key: KeyEvent, db: &Db) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Tab => self.screen = self.screen.next(),
            KeyCode::BackTab => self.screen = self.screen.previous(),
            KeyCode::Char('p') => {
                self.all_projects = !self.all_projects;
                self.refresh(db)?;
                self.note(if self.all_projects {
                    "showing every project".to_string()
                } else {
                    format!("showing {}", self.project)
                });
            }
            KeyCode::Char('/') => match self.screen {
                Screen::Memory => self.mode = Mode::Search,
                _ => self.mode = Mode::Filter,
            },
            _ => match self.screen {
                Screen::Queue => self.queue_key(key, db)?,
                Screen::Memory => self.memory_key(key, db)?,
                Screen::Swarm => self.swarm_key(key, db)?,
            },
        }
        Ok(())
    }

    fn queue_key(&mut self, key: KeyEvent, db: &Db) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('h') | KeyCode::Left => self.move_column(-1),
            KeyCode::Char('l') | KeyCode::Right => self.move_column(1),
            KeyCode::Char('g') | KeyCode::Home => self.selected[self.column.index()] = 0,
            KeyCode::Char('G') | KeyCode::End => {
                let last = self.column_tasks(self.column).len().saturating_sub(1);
                self.selected[self.column.index()] = last;
            }
            KeyCode::Char('a') => {
                self.mode = Mode::AddTask {
                    title: String::new(),
                    body: String::new(),
                    focus: 0,
                }
            }
            KeyCode::Enter => self.open_task_detail(db)?,
            KeyCode::Char('c') => self.act_on_selection(db, "cancel")?,
            KeyCode::Char('r') => self.act_on_selection(db, "reopen")?,
            KeyCode::Char('A') => {
                let Some(seq) = self.selected_task().map(|t| t.seq) else {
                    self.warn("nothing selected".to_string());
                    return Ok(());
                };
                match self.questions.get(&seq) {
                    Some(question) => {
                        self.mode = Mode::Answer {
                            seq,
                            question: question.question.clone(),
                            answer: String::new(),
                        };
                    }
                    None => self.warn(format!("task {seq} is not awaiting an answer")),
                }
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.clamp_selection();
            }
            _ => {}
        }
        Ok(())
    }

    fn swarm_key(&mut self, key: KeyEvent, db: &Db) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                let last = self.agents.len().saturating_sub(1);
                self.swarm_selected = (self.swarm_selected + 1).min(last);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.swarm_selected = self.swarm_selected.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => self.swarm_selected = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.swarm_selected = self.agents.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(seq) = self.selected_agent().map(|a| a.seq) {
                    self.show_task(seq, db)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn memory_key(&mut self, key: KeyEvent, db: &Db) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                let last = self.assertions.len().saturating_sub(1);
                self.memory_selected = (self.memory_selected + 1).min(last);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.memory_selected = self.memory_selected.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => self.memory_selected = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.memory_selected = self.assertions.len().saturating_sub(1);
            }
            KeyCode::Char('s') => {
                self.include_superseded = !self.include_superseded;
                self.refresh(db)?;
                self.note(if self.include_superseded {
                    "including superseded assertions".to_string()
                } else {
                    "showing current assertions only".to_string()
                });
            }
            KeyCode::Char('f') if self.footing().is_none() => {
                self.warn(
                    "no footing in this project — assertions are only anchored to files in a \
                     git checkout with `memory_footing` on"
                        .to_string(),
                );
            }
            KeyCode::Char('f') => {
                self.shaky_only = !self.shaky_only;
                self.memory_selected = 0;
                self.refresh(db)?;
                self.note(if self.shaky_only {
                    "showing only assertions whose files have moved".to_string()
                } else {
                    "showing every assertion".to_string()
                });
            }
            KeyCode::Enter => {
                if let Some(assertion) = self.selected_assertion().cloned() {
                    // Looked up here rather than on every poll: this is the one
                    // place it is drawn, and it is one query for one row.
                    self.voices.clear();
                    if let Some(sentence) = crate::footing::corroboration(db, &assertion) {
                        self.voices.insert(assertion.id.clone(), sentence);
                    }
                    self.mode = Mode::AssertionDetail {
                        assertion: Box::new(assertion),
                    };
                }
            }
            KeyCode::Char('d') => match self.selected_assertion() {
                Some(assertion) if assertion.superseded_by.is_some() => {
                    self.warn("that assertion is already superseded".to_string());
                }
                Some(assertion) => {
                    self.mode = Mode::Supersede {
                        id: assertion.id.clone(),
                        replacement: String::new(),
                    }
                }
                None => self.warn("nothing selected".to_string()),
            },
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.refresh(db)?;
            }
            _ => {}
        }
        Ok(())
    }

    // -------------------------------------------------------------- editing

    fn edit_filter(&mut self, key: KeyEvent, _db: &Db) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => {}
        }
        self.clamp_selection();
        Ok(())
    }

    fn edit_search(&mut self, key: KeyEvent, db: &Db) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.query.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) => self.query.push(c),
            _ => return Ok(()),
        }
        // Live search: every edit re-queries.
        self.memory_selected = 0;
        self.refresh(db)?;
        Ok(())
    }

    fn edit_add_task(
        &mut self,
        key: KeyEvent,
        mut title: String,
        mut body: String,
        mut focus: usize,
        db: &Db,
    ) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                return Ok(());
            }
            KeyCode::Tab | KeyCode::Down => focus = 1 - focus,
            KeyCode::BackTab | KeyCode::Up => focus = 1 - focus,
            KeyCode::Enter => {
                if title.trim().is_empty() {
                    self.warn("a task needs a title".to_string());
                    self.mode = Mode::AddTask { title, body, focus };
                    return Ok(());
                }
                let created = db
                    .tasks()
                    .create(&self.project, &title, &body, 0, ACTOR_TUI);
                self.mode = Mode::Normal;
                match created {
                    Ok(task) => {
                        crate::herald::announce_claimable(
                            self.herald.as_ref(),
                            db,
                            &self.config,
                            crate::herald::Cause::Filed,
                            task.seq,
                        );
                        self.refresh(db)?;
                        self.column = Column::Open;
                        self.note(format!("added task {}", task.seq));
                    }
                    Err(e) => self.warn(e.to_string()),
                }
                return Ok(());
            }
            KeyCode::Backspace => {
                if focus == 0 {
                    title.pop();
                } else {
                    body.pop();
                }
            }
            KeyCode::Char(c) => {
                if focus == 0 {
                    title.push(c);
                } else {
                    body.push(c);
                }
            }
            _ => {}
        }
        self.mode = Mode::AddTask { title, body, focus };
        Ok(())
    }

    fn edit_supersede(
        &mut self,
        key: KeyEvent,
        id: String,
        mut replacement: String,
        db: &Db,
    ) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                return Ok(());
            }
            KeyCode::Enter => {
                let outcome = db.memory().supersede(&id, &replacement, ACTOR_TUI);
                self.mode = Mode::Normal;
                match outcome {
                    Ok(_) => {
                        self.refresh(db)?;
                        self.note("assertion superseded".to_string());
                    }
                    Err(e) => self.warn(e.to_string()),
                }
                return Ok(());
            }
            KeyCode::Backspace => {
                replacement.pop();
            }
            KeyCode::Char(c) => replacement.push(c),
            _ => {}
        }
        self.mode = Mode::Supersede { id, replacement };
        Ok(())
    }

    fn edit_answer(
        &mut self,
        key: KeyEvent,
        seq: i64,
        question: String,
        mut answer: String,
        db: &Db,
    ) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                return Ok(());
            }
            KeyCode::Enter => {
                if answer.trim().is_empty() {
                    self.warn(
                        "answer must not be empty; say what the next agent should proceed with"
                            .to_string(),
                    );
                    self.mode = Mode::Answer {
                        seq,
                        question,
                        answer,
                    };
                    return Ok(());
                }
                let outcome = db.questions().answer(seq, ACTOR_TUI, &answer);
                self.mode = Mode::Normal;
                match outcome {
                    Ok(_) => {
                        crate::herald::announce_claimable(
                            self.herald.as_ref(),
                            db,
                            &self.config,
                            crate::herald::Cause::Answered,
                            seq,
                        );
                        self.refresh(db)?;
                        self.note(format!("answered task {seq}"));
                    }
                    Err(e) => self.warn(e.to_string()),
                }
                return Ok(());
            }
            KeyCode::Backspace => {
                answer.pop();
            }
            KeyCode::Char(c) => answer.push(c),
            _ => {}
        }
        self.mode = Mode::Answer {
            seq,
            question,
            answer,
        };
        Ok(())
    }

    // -------------------------------------------------------------- actions

    fn move_cursor(&mut self, delta: isize) {
        let len = self.column_tasks(self.column).len();
        if len == 0 {
            return;
        }
        let idx = &mut self.selected[self.column.index()];
        let next = (*idx as isize + delta).clamp(0, len as isize - 1);
        *idx = next as usize;
    }

    fn move_column(&mut self, delta: isize) {
        let current = self.column.index() as isize;
        let next = (current + delta).rem_euclid(Column::ALL.len() as isize);
        self.column = Column::ALL[next as usize];
    }

    fn open_task_detail(&mut self, db: &Db) -> anyhow::Result<()> {
        let Some(seq) = self.selected_task().map(|t| t.seq) else {
            return Ok(());
        };
        self.show_task(seq, db)
    }

    /// Load one task and everything around it into the detail overlay.
    fn show_task(&mut self, seq: i64, db: &Db) -> anyhow::Result<()> {
        let task = db.tasks().get(seq)?;
        let events = db.tasks().events(&task.id, 40)?;
        let learned = db.memory().for_task(&task.id)?;
        let (waiting_for, conflicts) = db.tasks().readiness(seq, self.config.clearance())?;
        let readiness = Readiness {
            waiting_for,
            ground: db.deps().ground(seq)?,
            blocks: db
                .deps()
                .dependents(seq)?
                .into_iter()
                .map(|b| b.seq)
                .collect(),
            paths: db.scopes().for_task(seq)?,
            conflicts,
            recusals: db.recusals().for_task(seq)?,
            verdicts: db.verdicts().for_task(seq)?,
            delivered: db.verdicts().of_review(seq)?,
            footprint: db.witnessed().footprint(seq).unwrap_or_default(),
            questions: db.questions().for_task(seq)?,
        };
        // Only what came from elsewhere: `learned` already holds this task's
        // own assertions, listed under their own heading.
        let recalled = db
            .recall()
            .for_task(seq, self.config.recall_limit())?
            .into_iter()
            .filter(|r| r.reason != crate::repo::RecallReason::SameTask)
            .collect();
        self.mode = Mode::TaskDetail {
            task: Box::new(task),
            events,
            learned,
            recalled,
            readiness: Box::new(readiness),
        };
        Ok(())
    }

    /// Cancel or reopen whatever the cursor is on.
    fn act_on_selection(&mut self, db: &Db, action: &str) -> anyhow::Result<()> {
        let Some(seq) = self.selected_task().map(|t| t.seq) else {
            self.warn("nothing selected".to_string());
            return Ok(());
        };
        let outcome = match action {
            "cancel" => db.tasks().cancel(seq, ACTOR_TUI, ""),
            _ => db.tasks().reopen(seq, ACTOR_TUI, ""),
        };
        match outcome {
            Ok(task) => {
                if action == "reopen" {
                    crate::herald::announce_claimable(
                        self.herald.as_ref(),
                        db,
                        &self.config,
                        crate::herald::Cause::Reopened,
                        task.seq,
                    );
                }
                self.refresh(db)?;
                self.note(format!("task {} is now {}", task.seq, task.status));
            }
            Err(e) => self.warn(e.to_string()),
        }
        Ok(())
    }

    fn note(&mut self, text: String) {
        self.toast = Some(Toast {
            text,
            is_error: false,
        });
    }

    fn warn(&mut self, text: String) {
        self.toast = Some(Toast {
            text,
            is_error: true,
        });
    }
}

/// Join live tasks to their declared files and cross-check them all.
///
/// Overlap is decided by pattern intersection, not by comparing strings, so
/// `src/**` and `src/db.rs` are recognised as the same territory even though
/// neither declaration mentions the other.
fn agent_rows(
    tasks: &[TaskSummary],
    declared: &[crate::model::ScopedTask],
    witnessed: &[crate::model::WitnessedTask],
    footprints: &BTreeMap<i64, Footprint>,
) -> Vec<AgentRow> {
    let scopes: BTreeMap<i64, &Vec<String>> =
        declared.iter().map(|s| (s.seq, &s.patterns)).collect();
    let moved: BTreeMap<i64, Vec<String>> = witnessed
        .iter()
        .map(|w| (w.seq, w.changes.iter().map(|c| c.path.clone()).collect()))
        .collect();
    let mut rows: Vec<AgentRow> = tasks
        .iter()
        .filter(|t| t.status.is_active())
        .map(|t| AgentRow {
            holder: t.claimed_by.clone().unwrap_or_else(|| "unknown".into()),
            seq: t.seq,
            title: t.title.clone(),
            status: t.status,
            lease_expires_at: t.lease_expires_at.clone(),
            patterns: scopes.get(&t.seq).map(|p| (*p).clone()).unwrap_or_default(),
            overlaps: Vec::new(),
            changed: moved.get(&t.seq).cloned().unwrap_or_default(),
            contentions: Vec::new(),
            shifted: Vec::new(),
            footprint: footprints.get(&t.seq).copied().unwrap_or_default(),
        })
        .collect();
    rows.sort_by(|a, b| a.holder.cmp(&b.holder).then(a.seq.cmp(&b.seq)));

    // Quadratic, over the handful of agents that can be live at once.
    for i in 0..rows.len() {
        let mut found = Vec::new();
        for (j, other) in rows.iter().enumerate() {
            if i == j {
                continue;
            }
            for pattern in &rows[i].patterns {
                for other_pattern in &other.patterns {
                    if glob::intersects(pattern, other_pattern) {
                        found.push(Overlap {
                            pattern: pattern.clone(),
                            other_seq: other.seq,
                            other_holder: other.holder.clone(),
                            other_pattern: other_pattern.clone(),
                        });
                    }
                }
            }
        }
        rows[i].overlaps = found;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use crate::repo::NewAssertion;

    const PROJECT: &str = "/tmp/project";

    fn fixture() -> (App, Db) {
        let db = Db::open_in_memory().unwrap();
        let app = App::new(
            PathBuf::from("/tmp/hird.db"),
            PROJECT.to_string(),
            Config::default(),
        );
        (app, db)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn seed(db: &Db, title: &str) -> i64 {
        db.tasks()
            .create(PROJECT, title, "body", 0, "cli")
            .unwrap()
            .seq
    }

    fn type_text(app: &mut App, db: &Db, text: &str) {
        for c in text.chars() {
            app.on_key(ch(c), db).unwrap();
        }
    }

    #[test]
    fn columns_partition_every_status_exactly_once() {
        for status in Status::ALL {
            let homes: Vec<_> = Column::ALL
                .into_iter()
                .filter(|c| c.accepts(status))
                .collect();
            assert_eq!(homes.len(), 1, "{status} lands in {homes:?}");
        }
    }

    #[test]
    fn tasks_sort_into_their_columns() {
        let (mut app, db) = fixture();
        seed(&db, "waiting");
        let claimed = seed(&db, "being worked");
        db.tasks()
            .claim(claimed, "codex:9f2c", Config::default().lease_ttl())
            .unwrap();
        let finished = seed(&db, "finished");
        db.tasks()
            .claim(finished, "codex:9f2c", Config::default().lease_ttl())
            .unwrap();
        db.tasks().complete(finished, "codex:9f2c", "ok").unwrap();
        let dropped = seed(&db, "dropped");
        db.tasks().cancel(dropped, "cli", "").unwrap();

        app.refresh(&db).unwrap();
        assert_eq!(app.column_tasks(Column::Open).len(), 1);
        assert_eq!(app.column_tasks(Column::Active).len(), 1);
        assert_eq!(app.column_tasks(Column::Done).len(), 1);
        assert_eq!(app.column_tasks(Column::Stopped).len(), 1);
    }

    #[test]
    fn q_quits_and_ctrl_c_quits_from_any_mode() {
        let (mut app, db) = fixture();
        app.on_key(ch('q'), &db).unwrap();
        assert!(app.should_quit);

        let (mut app, db) = fixture();
        app.on_key(ch('a'), &db).unwrap();
        assert!(matches!(app.mode, Mode::AddTask { .. }));
        app.on_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &db,
        )
        .unwrap();
        assert!(app.should_quit);
    }

    #[test]
    fn tab_switches_screens_and_question_mark_opens_help() {
        let (mut app, db) = fixture();
        app.on_key(key(KeyCode::Tab), &db).unwrap();
        assert_eq!(app.screen, Screen::Memory);
        app.on_key(key(KeyCode::Tab), &db).unwrap();
        assert_eq!(app.screen, Screen::Swarm);
        app.on_key(key(KeyCode::Tab), &db).unwrap();
        assert_eq!(app.screen, Screen::Queue);

        // Shift-Tab walks the same ring backwards.
        app.on_key(key(KeyCode::BackTab), &db).unwrap();
        assert_eq!(app.screen, Screen::Swarm);

        app.on_key(ch('?'), &db).unwrap();
        assert_eq!(app.mode, Mode::Help);
        app.on_key(key(KeyCode::Esc), &db).unwrap();
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn navigation_stays_inside_the_column() {
        let (mut app, db) = fixture();
        seed(&db, "one");
        seed(&db, "two");
        app.refresh(&db).unwrap();

        app.on_key(ch('k'), &db).unwrap();
        assert_eq!(
            app.selected_index(Column::Open),
            0,
            "k at the top is a no-op"
        );
        app.on_key(ch('j'), &db).unwrap();
        assert_eq!(app.selected_index(Column::Open), 1);
        app.on_key(ch('j'), &db).unwrap();
        assert_eq!(
            app.selected_index(Column::Open),
            1,
            "j at the bottom is a no-op"
        );
    }

    #[test]
    fn column_movement_wraps_around() {
        let (mut app, db) = fixture();
        assert_eq!(app.column, Column::Open);
        app.on_key(ch('h'), &db).unwrap();
        assert_eq!(app.column, Column::Stopped, "left from the first wraps");
        app.on_key(ch('l'), &db).unwrap();
        assert_eq!(app.column, Column::Open);
    }

    #[test]
    fn the_filter_matches_title_number_and_holder() {
        let (mut app, db) = fixture();
        seed(&db, "write the parser");
        let other = seed(&db, "fix the renderer");
        db.tasks()
            .claim(other, "codex:9f2c", Config::default().lease_ttl())
            .unwrap();
        app.refresh(&db).unwrap();

        app.on_key(ch('/'), &db).unwrap();
        assert_eq!(app.mode, Mode::Filter);
        type_text(&mut app, &db, "parser");
        assert_eq!(app.column_tasks(Column::Open).len(), 1);

        app.on_key(key(KeyCode::Esc), &db).unwrap();
        assert_eq!(app.filter, "");
        assert_eq!(app.mode, Mode::Normal);

        app.on_key(ch('/'), &db).unwrap();
        type_text(&mut app, &db, "codex");
        assert_eq!(app.column_tasks(Column::Active).len(), 1);
    }

    #[test]
    fn letters_typed_into_the_filter_are_literal_not_commands() {
        let (mut app, db) = fixture();
        seed(&db, "a task");
        app.refresh(&db).unwrap();

        app.on_key(ch('/'), &db).unwrap();
        type_text(&mut app, &db, "qa");
        assert!(!app.should_quit, "'q' in a text box must not quit");
        assert_eq!(app.filter, "qa");
    }

    #[test]
    fn adding_a_task_from_the_board_records_the_tui_as_the_actor() {
        let (mut app, db) = fixture();
        app.on_key(ch('a'), &db).unwrap();
        type_text(&mut app, &db, "new work");
        app.on_key(key(KeyCode::Tab), &db).unwrap();
        type_text(&mut app, &db, "details");
        app.on_key(key(KeyCode::Enter), &db).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.column_tasks(Column::Open).len(), 1);

        let task = db.tasks().get(1).unwrap();
        assert_eq!(task.title, "new work");
        assert_eq!(task.body, "details");
        let events = db.tasks().events(&task.id, 10).unwrap();
        assert_eq!(events[0].actor, "tui");
        assert_eq!(app.toast.as_ref().unwrap().text, "added task 1");
    }

    #[test]
    fn an_empty_title_keeps_the_prompt_open_and_complains() {
        let (mut app, db) = fixture();
        app.on_key(ch('a'), &db).unwrap();
        app.on_key(key(KeyCode::Enter), &db).unwrap();

        assert!(matches!(app.mode, Mode::AddTask { .. }));
        assert!(app.toast.as_ref().unwrap().is_error);
        assert_eq!(db.tasks().list(&app.scope(), None).unwrap().len(), 0);
    }

    #[test]
    fn escape_abandons_the_add_prompt() {
        let (mut app, db) = fixture();
        app.on_key(ch('a'), &db).unwrap();
        type_text(&mut app, &db, "never mind");
        app.on_key(key(KeyCode::Esc), &db).unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(db.tasks().list(&app.scope(), None).unwrap().len(), 0);
    }

    #[test]
    fn answering_from_the_board_makes_the_task_workable_again() {
        let (mut app, db) = fixture();
        let seq = seed(&db, "choose compatibility");
        db.tasks()
            .claim(seq, "codex:1", Config::default().lease_ttl())
            .unwrap();
        db.tasks()
            .release_asking(
                seq,
                "codex:1",
                "implementation point isolated",
                "Preserve the legacy format?",
            )
            .unwrap();
        app.refresh(&db).unwrap();
        assert!(app.workable().is_empty());

        app.on_key(ch('A'), &db).unwrap();
        assert!(matches!(app.mode, Mode::Answer { .. }));
        type_text(&mut app, &db, "No; migrate it.");
        app.on_key(key(KeyCode::Enter), &db).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.workable().iter().map(|t| t.seq).collect::<Vec<_>>(),
            vec![seq]
        );
        let history = db.questions().for_task(seq).unwrap();
        assert_eq!(history[0].answer.as_deref(), Some("No; migrate it."));
        assert_eq!(history[0].answered_by.as_deref(), Some("tui"));
    }

    #[test]
    fn cancel_and_reopen_act_on_the_selected_card() {
        let (mut app, db) = fixture();
        seed(&db, "doomed");
        app.refresh(&db).unwrap();

        app.on_key(ch('c'), &db).unwrap();
        assert_eq!(db.tasks().get(1).unwrap().status, Status::Cancelled);
        assert_eq!(app.toast.as_ref().unwrap().text, "task 1 is now cancelled");

        // The card moved to the last column; follow it and reopen.
        app.column = Column::Stopped;
        app.on_key(ch('r'), &db).unwrap();
        assert_eq!(db.tasks().get(1).unwrap().status, Status::Open);
    }

    #[test]
    fn an_illegal_action_surfaces_the_repository_error() {
        let (mut app, db) = fixture();
        seed(&db, "open task");
        app.refresh(&db).unwrap();

        app.on_key(ch('r'), &db).unwrap();
        let toast = app.toast.as_ref().unwrap();
        assert!(toast.is_error);
        assert_eq!(toast.text, "cannot reopen task 1: it is open");
    }

    #[test]
    fn acting_with_nothing_selected_says_so() {
        let (mut app, db) = fixture();
        app.refresh(&db).unwrap();
        app.on_key(ch('c'), &db).unwrap();
        assert_eq!(app.toast.as_ref().unwrap().text, "nothing selected");
    }

    #[test]
    fn enter_opens_the_task_detail_with_body_history_and_assertions() {
        let (mut app, db) = fixture();
        let seq = seed(&db, "write the parser");
        let task = db.tasks().get(seq).unwrap();
        db.memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "the lexer is hand written",
                tags: "",
                actor: "codex:9f2c",
                task_seq: Some(seq),
            })
            .unwrap();
        app.refresh(&db).unwrap();

        app.on_key(key(KeyCode::Enter), &db).unwrap();
        match &app.mode {
            Mode::TaskDetail {
                task: shown,
                events,
                learned,
                ..
            } => {
                assert_eq!(shown.id, task.id);
                assert_eq!(shown.body, "body");
                assert!(!events.is_empty());
                assert_eq!(learned.len(), 1);
            }
            other => panic!("expected the detail pane, got {other:?}"),
        }
        app.on_key(key(KeyCode::Esc), &db).unwrap();
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn the_project_toggle_widens_and_narrows_the_scope() {
        let (mut app, db) = fixture();
        seed(&db, "here");
        db.tasks()
            .create("/elsewhere", "there", "", 0, "cli")
            .unwrap();
        app.refresh(&db).unwrap();
        assert_eq!(app.tasks.len(), 1);

        app.on_key(ch('p'), &db).unwrap();
        assert!(app.all_projects);
        assert_eq!(app.tasks.len(), 2);
        assert_eq!(app.toast.as_ref().unwrap().text, "showing every project");

        app.on_key(ch('p'), &db).unwrap();
        assert_eq!(app.tasks.len(), 1);
    }

    #[test]
    fn the_memory_search_box_filters_as_you_type() {
        let (mut app, db) = fixture();
        for content in ["the lexer lives in src/lex.rs", "the build uses just"] {
            db.memory()
                .store(NewAssertion {
                    project: PROJECT,
                    content,
                    tags: "",
                    actor: "codex:9f2c",
                    task_seq: None,
                })
                .unwrap();
        }
        app.refresh(&db).unwrap();
        app.screen = Screen::Memory;
        assert_eq!(app.assertions.len(), 2);

        app.on_key(ch('/'), &db).unwrap();
        assert_eq!(app.mode, Mode::Search);
        type_text(&mut app, &db, "lexer");
        assert_eq!(app.assertions.len(), 1);

        app.on_key(key(KeyCode::Backspace), &db).unwrap();
        type_text(&mut app, &db, "r");
        assert_eq!(app.assertions.len(), 1);

        app.on_key(key(KeyCode::Esc), &db).unwrap();
        assert_eq!(app.query, "");
        assert_eq!(app.assertions.len(), 2);
    }

    #[test]
    fn superseding_from_the_browser_writes_a_replacement_authored_by_tui() {
        let (mut app, db) = fixture();
        db.memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "the api listens on 8080",
                tags: "api",
                actor: "codex:9f2c",
                task_seq: None,
            })
            .unwrap();
        app.refresh(&db).unwrap();
        app.screen = Screen::Memory;

        app.on_key(ch('d'), &db).unwrap();
        assert!(matches!(app.mode, Mode::Supersede { .. }));
        type_text(&mut app, &db, "the api listens on 9090");
        app.on_key(key(KeyCode::Enter), &db).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.assertions.len(), 1);
        assert_eq!(app.assertions[0].content, "the api listens on 9090");
        assert_eq!(app.assertions[0].actor, "tui");
        assert_eq!(app.toast.as_ref().unwrap().text, "assertion superseded");
    }

    #[test]
    fn an_already_superseded_assertion_cannot_be_superseded_again() {
        let (mut app, db) = fixture();
        let original = db
            .memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "stale",
                tags: "",
                actor: "codex:9f2c",
                task_seq: None,
            })
            .unwrap();
        db.memory().supersede(&original.id, "fresh", "tui").unwrap();

        app.include_superseded = true;
        app.refresh(&db).unwrap();
        app.screen = Screen::Memory;
        // Order is newest first, so index 1 is the superseded original.
        app.memory_selected = 1;
        assert!(app.selected_assertion().unwrap().superseded_by.is_some());

        app.on_key(ch('d'), &db).unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.toast.as_ref().unwrap().is_error);
    }

    #[test]
    fn the_superseded_toggle_changes_what_is_listed() {
        let (mut app, db) = fixture();
        let original = db
            .memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "stale",
                tags: "",
                actor: "codex:9f2c",
                task_seq: None,
            })
            .unwrap();
        db.memory().supersede(&original.id, "fresh", "tui").unwrap();
        app.refresh(&db).unwrap();
        app.screen = Screen::Memory;
        assert_eq!(app.assertions.len(), 1);

        app.on_key(ch('s'), &db).unwrap();
        assert_eq!(app.assertions.len(), 2);
    }

    #[test]
    fn selections_survive_rows_disappearing_underneath_them() {
        let (mut app, db) = fixture();
        for i in 0..3 {
            seed(&db, &format!("task {i}"));
        }
        app.refresh(&db).unwrap();
        app.on_key(ch('G'), &db).unwrap();
        assert_eq!(app.selected_index(Column::Open), 2);

        // Another agent takes two of them while we are looking.
        for seq in [2, 3] {
            db.tasks()
                .claim(seq, "codex:9f2c", Config::default().lease_ttl())
                .unwrap();
        }
        app.refresh(&db).unwrap();
        assert_eq!(app.selected_index(Column::Open), 0);
        assert!(app.selected_task().is_some());
    }

    #[test]
    fn the_poll_clock_reports_staleness() {
        let (mut app, db) = fixture();
        app.refresh(&db).unwrap();
        let now = app.last_poll;
        assert!(!app.due_for_refresh(now));
        assert!(app.due_for_refresh(now + chrono::Duration::milliseconds(600)));
    }

    #[test]
    fn text_entry_modes_are_flagged_so_keys_are_not_treated_as_commands() {
        assert!(!Mode::Normal.is_text_entry());
        assert!(!Mode::Help.is_text_entry());
        assert!(Mode::Filter.is_text_entry());
        assert!(Mode::Search.is_text_entry());
        assert!(Mode::AddTask {
            title: String::new(),
            body: String::new(),
            focus: 0
        }
        .is_text_entry());
        assert!(Mode::Answer {
            seq: 1,
            question: String::new(),
            answer: String::new(),
        }
        .is_text_entry());
    }

    /// Wait for the detached hook to write, or fail loudly.
    fn wait_for(path: &std::path::Path, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(contents) = std::fs::read_to_string(path) {
                if contents.contains(needle) {
                    return contents;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the board never announced {needle:?} to {}",
                path.display(),
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The board is a writer as well as a reader: tasks filed or reopened
    /// through it must wake the same workers as their CLI equivalents.
    #[test]
    fn the_board_announces_tasks_it_files_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("summons");
        let config = Config {
            dispatch_hook: format!(
                "printf '%s %s\\n' \"$HIRD_EVENT\" \"$HIRD_TASK\" >> {}",
                log.display()
            ),
            ..Config::default()
        };
        let db = Db::open_in_memory().unwrap();
        let herald = config.herald(std::path::Path::new("/tmp/hird.db"));
        let mut app = App::new(PathBuf::from("/tmp/hird.db"), PROJECT.to_string(), config)
            .with_herald(herald);
        app.refresh(&db).unwrap();

        app.on_key(ch('a'), &db).unwrap();
        type_text(&mut app, &db, "new work");
        app.on_key(key(KeyCode::Enter), &db).unwrap();
        wait_for(&log, "filed 1");

        app.on_key(ch('c'), &db).unwrap();
        app.column = Column::Stopped;
        app.on_key(ch('r'), &db).unwrap();
        let contents = wait_for(&log, "reopened 1");
        assert_eq!(
            contents.lines().collect::<Vec<_>>(),
            ["filed 1", "reopened 1"]
        );
    }

    /// The board polls twice a second, so in a swarm where the agents have
    /// stopped calling it is the process that collects a dead holder's lease
    /// — and a sweep reports each lapsed lease to exactly one sweeper. A board
    /// that only swept would therefore be the reason nobody is ever summoned
    /// to pick the work back up.
    #[test]
    fn the_board_announces_the_expiries_its_own_poll_collects() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("summons");
        let config = Config {
            dispatch_hook: format!(
                "printf '%s %s' \"$HIRD_EVENT\" \"$HIRD_TASK\" > {}",
                log.display()
            ),
            ..Config::default()
        };

        let db = Db::open_in_memory().unwrap();
        let seq = seed(&db, "held by an agent that stopped answering");
        db.tasks()
            .claim(seq, "codex:9f2c", config.lease_ttl())
            .unwrap();
        // Back-dated rather than claimed with a zero TTL, which would land in
        // the same millisecond as the sweep's own `now`.
        db.conn()
            .execute(
                "UPDATE tasks SET lease_expires_at = ?1 WHERE seq = ?2",
                rusqlite::params![
                    crate::model::fmt_ts(Utc::now() - chrono::Duration::hours(1)),
                    seq
                ],
            )
            .unwrap();

        let herald = config.herald(std::path::Path::new("/tmp/hird.db"));
        assert!(herald.is_some(), "the test config configures a hook");
        let mut app = App::new(PathBuf::from("/tmp/hird.db"), PROJECT.to_string(), config)
            .with_herald(herald);
        app.refresh(&db).unwrap();

        assert_eq!(
            wait_for(&log, &format!("lease_expired {seq}")),
            format!("lease_expired {seq}")
        );
        assert_eq!(
            app.tasks.iter().find(|t| t.seq == seq).map(|t| t.status),
            Some(Status::Open),
            "the expiry itself still happens"
        );
    }

    /// A board with no hook configured must not pay for one, and must not
    /// change what it shows: the expiry happens either way.
    #[test]
    fn a_board_without_a_hook_still_expires_the_lease() {
        let (mut app, db) = fixture();
        let seq = seed(&db, "held by an agent that stopped answering");
        db.tasks()
            .claim(seq, "codex:9f2c", Config::default().lease_ttl())
            .unwrap();
        db.conn()
            .execute(
                "UPDATE tasks SET lease_expires_at = ?1 WHERE seq = ?2",
                rusqlite::params![
                    crate::model::fmt_ts(Utc::now() - chrono::Duration::hours(1)),
                    seq
                ],
            )
            .unwrap();

        app.refresh(&db).unwrap();

        assert_eq!(
            app.tasks.iter().find(|t| t.seq == seq).map(|t| t.status),
            Some(Status::Open)
        );
    }
}
