use crate::models::{
    ApprovalMode, Message, MessageRole, Project, Quest, QuestStatus, Session, SessionStatus,
    ToolCallRecord, Workspace,
};
use crate::skills::{
    Skill, SkillAssignment, SkillDeployment, SkillDeploymentMethod, SkillDeploymentStatus,
    SkillScope, SkillSourceType,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use uuid::Uuid;

pub struct Storage {
    conn: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite DB at {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;",
        )?;
        let storage = Self {
            conn: Mutex::new(conn),
        };
        storage.migrate()?;
        Ok(storage)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let storage = Self {
            conn: Mutex::new(conn),
        };
        storage.migrate()?;
        Ok(storage)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS workspaces (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                workdir TEXT NOT NULL,
                instructions TEXT NOT NULL DEFAULT '',
                pinned INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS quests (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                title TEXT NOT NULL,
                goal TEXT NOT NULL DEFAULT '',
                instructions TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'active',
                pinned INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                quest_id TEXT REFERENCES quests(id) ON DELETE SET NULL,
                title TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                model TEXT,
                external_session_id TEXT,
                status TEXT NOT NULL DEFAULT 'idle',
                pinned INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                thinking TEXT,
                tool_calls_json TEXT NOT NULL DEFAULT '[]',
                provider_id TEXT,
                model TEXT,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_projects_workspace ON projects(workspace_id);
            CREATE INDEX IF NOT EXISTS idx_quests_project ON quests(project_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project_id);
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, created_at);

            CREATE TABLE IF NOT EXISTS skills (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                scope TEXT NOT NULL,
                project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
                path TEXT NOT NULL,
                source_type TEXT NOT NULL DEFAULT 'manual',
                source_uri TEXT,
                source_ref TEXT,
                source_revision TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                content_hash TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS skill_assignments (
                skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
                project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                enabled INTEGER NOT NULL,
                PRIMARY KEY (skill_id, project_id)
            );

            CREATE TABLE IF NOT EXISTS skill_deployments (
                skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
                provider_id TEXT NOT NULL,
                project_id TEXT NOT NULL DEFAULT '',
                execution_target TEXT NOT NULL,
                deployment_path TEXT NOT NULL DEFAULT '',
                deployment_method TEXT NOT NULL,
                deployed_hash TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                last_error TEXT,
                last_synced_at TEXT,
                PRIMARY KEY (skill_id, provider_id, project_id, execution_target)
            );

            CREATE INDEX IF NOT EXISTS idx_skills_scope_project ON skills(scope, project_id);
            CREATE INDEX IF NOT EXISTS idx_skill_assignments_project ON skill_assignments(project_id);
            CREATE INDEX IF NOT EXISTS idx_skill_deployments_provider ON skill_deployments(provider_id, project_id);
            ",
        )?;
        // Columns added after the first release.
        let has_effort = conn
            .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'effort'")?
            .exists([])?;
        if !has_effort {
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN effort TEXT;")?;
        }
        let has_mode = conn
            .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'approval_mode'")?
            .exists([])?;
        if !has_mode {
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN approval_mode TEXT;")?;
        }
        Ok(())
    }

    pub fn ensure_default_workspace(&self) -> Result<Workspace> {
        let workspaces = self.list_workspaces()?;
        if let Some(ws) = workspaces.into_iter().next() {
            return Ok(ws);
        }
        self.create_workspace("Default Workspace")
    }

    pub fn create_workspace(&self, name: &str) -> Result<Workspace> {
        let now = Utc::now();
        let ws = Workspace {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            created_at: now,
            updated_at: now,
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO workspaces (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                ws.id,
                ws.name,
                ws.created_at.to_rfc3339(),
                ws.updated_at.to_rfc3339()
            ],
        )?;
        Ok(ws)
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, created_at, updated_at FROM workspaces ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Workspace {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: parse_dt(r.get::<_, String>(2)?),
                updated_at: parse_dt(r.get::<_, String>(3)?),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn create_project(
        &self,
        workspace_id: &str,
        name: &str,
        workdir: &str,
        description: &str,
    ) -> Result<Project> {
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4().to_string(),
            workspace_id: workspace_id.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            workdir: workdir.to_string(),
            instructions: String::new(),
            pinned: false,
            archived: false,
            created_at: now,
            updated_at: now,
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, description, workdir, instructions, pinned, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                project.id,
                project.workspace_id,
                project.name,
                project.description,
                project.workdir,
                project.instructions,
                project.pinned as i64,
                project.archived as i64,
                project.created_at.to_rfc3339(),
                project.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(project)
    }

    pub fn ensure_project_for_path(&self, workspace_id: &str, workdir: &Path) -> Result<Project> {
        let canon = workdir
            .canonicalize()
            .unwrap_or_else(|_| workdir.to_path_buf());
        let workdir_str = crate::paths::simplify_path(&canon.to_string_lossy());
        let projects = self.list_projects(workspace_id)?;
        if let Some(existing) = projects.into_iter().find(|p| p.workdir == workdir_str) {
            return Ok(existing);
        }
        let name = Path::new(&workdir_str)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Project".to_string());
        self.create_project(workspace_id, &name, &workdir_str, "")
    }

    pub fn list_projects(&self, workspace_id: &str) -> Result<Vec<Project>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, name, description, workdir, instructions, pinned, archived, created_at, updated_at
             FROM projects WHERE workspace_id = ?1 AND archived = 0
             ORDER BY pinned DESC, updated_at DESC",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| {
            Ok(Project {
                id: r.get(0)?,
                workspace_id: r.get(1)?,
                name: r.get(2)?,
                description: r.get(3)?,
                // Older rows may hold a Windows verbatim path (`\\?\...`).
                workdir: crate::paths::simplify_path(&r.get::<_, String>(4)?),
                instructions: r.get(5)?,
                pinned: r.get::<_, i64>(6)? != 0,
                archived: r.get::<_, i64>(7)? != 0,
                created_at: parse_dt(r.get::<_, String>(8)?),
                updated_at: parse_dt(r.get::<_, String>(9)?),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn delete_project(&self, project_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM projects WHERE id = ?1", params![project_id])?;
        Ok(())
    }

    pub fn create_quest(&self, project_id: &str, title: &str, goal: &str) -> Result<Quest> {
        let now = Utc::now();
        let quest = Quest {
            id: Uuid::new_v4().to_string(),
            project_id: project_id.to_string(),
            title: title.to_string(),
            goal: goal.to_string(),
            instructions: String::new(),
            status: QuestStatus::Active,
            pinned: false,
            created_at: now,
            updated_at: now,
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO quests (id, project_id, title, goal, instructions, status, pinned, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                quest.id,
                quest.project_id,
                quest.title,
                quest.goal,
                quest.instructions,
                quest.status.as_str(),
                quest.pinned as i64,
                quest.created_at.to_rfc3339(),
                quest.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(quest)
    }

    pub fn list_quests(&self, project_id: &str) -> Result<Vec<Quest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, project_id, title, goal, instructions, status, pinned, created_at, updated_at
             FROM quests WHERE project_id = ?1
             ORDER BY pinned DESC, updated_at DESC",
        )?;
        let rows = stmt.query_map(params![project_id], |r| {
            Ok(Quest {
                id: r.get(0)?,
                project_id: r.get(1)?,
                title: r.get(2)?,
                goal: r.get(3)?,
                instructions: r.get(4)?,
                status: QuestStatus::from_str(&r.get::<_, String>(5)?),
                pinned: r.get::<_, i64>(6)? != 0,
                created_at: parse_dt(r.get::<_, String>(7)?),
                updated_at: parse_dt(r.get::<_, String>(8)?),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn create_session(
        &self,
        project_id: &str,
        quest_id: Option<&str>,
        title: &str,
        provider_id: &str,
        model: Option<&str>,
    ) -> Result<Session> {
        let now = Utc::now();
        let session = Session {
            id: Uuid::new_v4().to_string(),
            project_id: project_id.to_string(),
            quest_id: quest_id.map(str::to_string),
            title: title.to_string(),
            provider_id: provider_id.to_string(),
            model: model.map(str::to_string),
            effort: None,
            approval_mode: None,
            external_session_id: None,
            status: SessionStatus::Idle,
            pinned: false,
            archived: false,
            created_at: now,
            updated_at: now,
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id, project_id, quest_id, title, provider_id, model, external_session_id, status, pinned, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                session.id,
                session.project_id,
                session.quest_id,
                session.title,
                session.provider_id,
                session.model,
                session.external_session_id,
                session.status.as_str(),
                session.pinned as i64,
                session.archived as i64,
                session.created_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(session)
    }

    pub fn list_sessions(&self, project_id: &str) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, project_id, quest_id, title, provider_id, model, external_session_id, status, pinned, archived, created_at, updated_at, effort, approval_mode
             FROM sessions WHERE project_id = ?1 AND archived = 0
             ORDER BY pinned DESC, updated_at DESC",
        )?;
        let rows = stmt.query_map(params![project_id], |r| {
            Ok(Session {
                id: r.get(0)?,
                project_id: r.get(1)?,
                quest_id: r.get(2)?,
                title: r.get(3)?,
                provider_id: r.get(4)?,
                model: r.get(5)?,
                external_session_id: r.get(6)?,
                status: SessionStatus::from_str(&r.get::<_, String>(7)?),
                pinned: r.get::<_, i64>(8)? != 0,
                archived: r.get::<_, i64>(9)? != 0,
                created_at: parse_dt(r.get::<_, String>(10)?),
                updated_at: parse_dt(r.get::<_, String>(11)?),
                effort: r.get(12)?,
                approval_mode: r
                    .get::<_, Option<String>>(13)?
                    .and_then(|m| ApprovalMode::parse(&m)),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, project_id, quest_id, title, provider_id, model, external_session_id, status, pinned, archived, created_at, updated_at, effort, approval_mode
             FROM sessions WHERE id = ?1",
            params![session_id],
            |r| {
                Ok(Session {
                    id: r.get(0)?,
                    project_id: r.get(1)?,
                    quest_id: r.get(2)?,
                    title: r.get(3)?,
                    provider_id: r.get(4)?,
                    model: r.get(5)?,
                    external_session_id: r.get(6)?,
                    status: SessionStatus::from_str(&r.get::<_, String>(7)?),
                    pinned: r.get::<_, i64>(8)? != 0,
                    archived: r.get::<_, i64>(9)? != 0,
                    created_at: parse_dt(r.get::<_, String>(10)?),
                    updated_at: parse_dt(r.get::<_, String>(11)?),
                    effort: r.get(12)?,
                    approval_mode: r
                        .get::<_, Option<String>>(13)?
                        .and_then(|m| ApprovalMode::parse(&m)),
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn set_session_approval_mode(
        &self,
        session_id: &str,
        mode: Option<ApprovalMode>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET approval_mode = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                mode.map(|m| m.as_str()),
                Utc::now().to_rfc3339(),
                session_id
            ],
        )?;
        Ok(())
    }

    pub fn set_session_effort(&self, session_id: &str, effort: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET effort = ?1, updated_at = ?2 WHERE id = ?3",
            params![effort, Utc::now().to_rfc3339(), session_id],
        )?;
        Ok(())
    }

    pub fn update_session_meta(
        &self,
        session_id: &str,
        title: Option<&str>,
        provider_id: Option<&str>,
        model: Option<Option<&str>>,
        external_session_id: Option<Option<&str>>,
        status: Option<SessionStatus>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        if let Some(t) = title {
            conn.execute(
                "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE id = ?3",
                params![t, now, session_id],
            )?;
        }
        if let Some(p) = provider_id {
            conn.execute(
                "UPDATE sessions SET provider_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![p, now, session_id],
            )?;
        }
        if let Some(m) = model {
            conn.execute(
                "UPDATE sessions SET model = ?1, updated_at = ?2 WHERE id = ?3",
                params![m, now, session_id],
            )?;
        }
        if let Some(ext) = external_session_id {
            conn.execute(
                "UPDATE sessions SET external_session_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![ext, now, session_id],
            )?;
        }
        if let Some(st) = status {
            conn.execute(
                "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![st.as_str(), now, session_id],
            )?;
        }
        Ok(())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
        Ok(())
    }

    /// Marks sessions left in an active state (e.g. by a crash or quitting
    /// mid-run) as interrupted. No agent process survives an app restart, so
    /// those sessions can never complete on their own. Returns rows changed.
    pub fn reset_active_sessions(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE sessions SET status = 'interrupted'
             WHERE status IN ('running', 'waiting_for_approval', 'waiting_for_subagent')",
            [],
        )?;
        Ok(n)
    }

    pub fn delete_quest(&self, quest_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM quests WHERE id = ?1", params![quest_id])?;
        Ok(())
    }

    pub fn append_message(&self, msg: &Message) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tools_json = serde_json::to_string(&msg.tool_calls)?;
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, thinking, tool_calls_json, provider_id, model, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                msg.id,
                msg.session_id,
                msg.role.as_str(),
                msg.content,
                msg.thinking,
                tools_json,
                msg.provider_id,
                msg.model,
                msg.created_at.to_rfc3339(),
            ],
        )?;
        conn.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), msg.session_id],
        )?;
        Ok(())
    }

    pub fn list_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, role, content, thinking, tool_calls_json, provider_id, model, created_at
             FROM messages WHERE session_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            let tools_str: String = r.get(5)?;
            let tool_calls: Vec<ToolCallRecord> =
                serde_json::from_str(&tools_str).unwrap_or_default();
            Ok(Message {
                id: r.get(0)?,
                session_id: r.get(1)?,
                role: MessageRole::from_str(&r.get::<_, String>(2)?),
                content: r.get(3)?,
                thinking: r.get(4)?,
                tool_calls,
                provider_id: r.get(6)?,
                model: r.get(7)?,
                created_at: parse_dt(r.get::<_, String>(8)?),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // ---- Skills persistence ----

    pub fn insert_skill(&self, skill: &Skill) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO skills (
                id, name, description, scope, project_id, path,
                source_type, source_uri, source_ref, source_revision,
                enabled, content_hash, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                skill.id,
                skill.name,
                skill.description,
                skill.scope.as_str(),
                skill.project_id,
                skill.path,
                skill.source_type.as_str(),
                skill.source_uri,
                skill.source_ref,
                skill.source_revision,
                skill.enabled as i64,
                skill.content_hash,
                skill.created_at.to_rfc3339(),
                skill.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn update_skill(&self, skill: &Skill) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE skills SET
                name = ?2,
                description = ?3,
                scope = ?4,
                project_id = ?5,
                path = ?6,
                source_type = ?7,
                source_uri = ?8,
                source_ref = ?9,
                source_revision = ?10,
                enabled = ?11,
                content_hash = ?12,
                updated_at = ?13
             WHERE id = ?1",
            params![
                skill.id,
                skill.name,
                skill.description,
                skill.scope.as_str(),
                skill.project_id,
                skill.path,
                skill.source_type.as_str(),
                skill.source_uri,
                skill.source_ref,
                skill.source_revision,
                skill.enabled as i64,
                skill.content_hash,
                skill.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn delete_skill(&self, skill_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM skills WHERE id = ?1", params![skill_id])?;
        Ok(())
    }

    pub fn get_skill(&self, skill_id: &str) -> Result<Option<Skill>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, description, scope, project_id, path,
                    source_type, source_uri, source_ref, source_revision,
                    enabled, content_hash, created_at, updated_at
             FROM skills WHERE id = ?1",
            params![skill_id],
            row_to_skill,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_skills(&self) -> Result<Vec<Skill>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, scope, project_id, path,
                    source_type, source_uri, source_ref, source_revision,
                    enabled, content_hash, created_at, updated_at
             FROM skills
             ORDER BY scope ASC, name ASC",
        )?;
        let rows = stmt.query_map([], row_to_skill)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn upsert_skill_assignment(&self, assignment: &SkillAssignment) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO skill_assignments (skill_id, project_id, enabled)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(skill_id, project_id) DO UPDATE SET enabled = excluded.enabled",
            params![
                assignment.skill_id,
                assignment.project_id,
                assignment.enabled as i64,
            ],
        )?;
        Ok(())
    }

    pub fn delete_skill_assignment(&self, skill_id: &str, project_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM skill_assignments WHERE skill_id = ?1 AND project_id = ?2",
            params![skill_id, project_id],
        )?;
        Ok(())
    }

    pub fn list_skill_assignments(&self, project_id: &str) -> Result<Vec<SkillAssignment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT skill_id, project_id, enabled
             FROM skill_assignments WHERE project_id = ?1",
        )?;
        let rows = stmt.query_map(params![project_id], |r| {
            Ok(SkillAssignment {
                skill_id: r.get(0)?,
                project_id: r.get(1)?,
                enabled: r.get::<_, i64>(2)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn list_all_skill_assignments(&self) -> Result<Vec<SkillAssignment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT skill_id, project_id, enabled FROM skill_assignments")?;
        let rows = stmt.query_map([], |r| {
            Ok(SkillAssignment {
                skill_id: r.get(0)?,
                project_id: r.get(1)?,
                enabled: r.get::<_, i64>(2)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn upsert_skill_deployment(&self, dep: &SkillDeployment) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO skill_deployments (
                skill_id, provider_id, project_id, execution_target,
                deployment_path, deployment_method, deployed_hash,
                status, last_error, last_synced_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(skill_id, provider_id, project_id, execution_target) DO UPDATE SET
                deployment_path = excluded.deployment_path,
                deployment_method = excluded.deployment_method,
                deployed_hash = excluded.deployed_hash,
                status = excluded.status,
                last_error = excluded.last_error,
                last_synced_at = excluded.last_synced_at",
            params![
                dep.skill_id,
                dep.provider_id,
                dep.project_id,
                dep.execution_target,
                dep.deployment_path,
                dep.deployment_method.as_str(),
                dep.deployed_hash,
                dep.status.as_str(),
                dep.last_error,
                dep.last_synced_at.map(|d| d.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn list_skill_deployments(&self, skill_id: Option<&str>) -> Result<Vec<SkillDeployment>> {
        let conn = self.conn.lock().unwrap();
        let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<SkillDeployment> {
            Ok(SkillDeployment {
                skill_id: r.get(0)?,
                provider_id: r.get(1)?,
                project_id: r.get(2)?,
                execution_target: r.get(3)?,
                deployment_path: r.get(4)?,
                deployment_method: SkillDeploymentMethod::from_str(&r.get::<_, String>(5)?),
                deployed_hash: r.get(6)?,
                status: SkillDeploymentStatus::from_str(&r.get::<_, String>(7)?),
                last_error: r.get(8)?,
                last_synced_at: r.get::<_, Option<String>>(9)?.map(parse_dt),
            })
        };
        if let Some(sid) = skill_id {
            let mut stmt = conn.prepare(
                "SELECT skill_id, provider_id, project_id, execution_target,
                        deployment_path, deployment_method, deployed_hash,
                        status, last_error, last_synced_at
                 FROM skill_deployments WHERE skill_id = ?1
                 ORDER BY provider_id ASC, execution_target ASC",
            )?;
            let rows = stmt.query_map(params![sid], map_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        } else {
            let mut stmt = conn.prepare(
                "SELECT skill_id, provider_id, project_id, execution_target,
                        deployment_path, deployment_method, deployed_hash,
                        status, last_error, last_synced_at
                 FROM skill_deployments
                 ORDER BY provider_id ASC, execution_target ASC",
            )?;
            let rows = stmt.query_map([], map_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        }
    }

    pub fn delete_skill_deployments_for_provider(
        &self,
        provider_id: &str,
        project_id: &str,
        execution_target: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM skill_deployments WHERE provider_id = ?1 AND project_id = ?2 AND execution_target = ?3",
            params![provider_id, project_id, execution_target],
        )?;
        Ok(())
    }
}

fn row_to_skill(r: &rusqlite::Row<'_>) -> rusqlite::Result<Skill> {
    Ok(Skill {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        scope: SkillScope::from_str(&r.get::<_, String>(3)?),
        project_id: r.get(4)?,
        path: r.get(5)?,
        source_type: SkillSourceType::from_str(&r.get::<_, String>(6)?),
        source_uri: r.get(7)?,
        source_ref: r.get(8)?,
        source_revision: r.get(9)?,
        enabled: r.get::<_, i64>(10)? != 0,
        content_hash: r.get(11)?,
        created_at: parse_dt(r.get::<_, String>(12)?),
        updated_at: parse_dt(r.get::<_, String>(13)?),
    })
}

fn parse_dt(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn session_effort_round_trips() -> Result<()> {
        let storage = Storage::open_in_memory()?;
        let ws = storage.ensure_default_workspace()?;
        let project = storage.ensure_project_for_path(&ws.id, &PathBuf::from("/tmp/same-e"))?;
        let s = storage.create_session(&project.id, None, "t", "codex", None)?;
        assert_eq!(s.effort, None);
        storage.set_session_effort(&s.id, Some("high"))?;
        assert_eq!(
            storage.get_session(&s.id)?.unwrap().effort.as_deref(),
            Some("high")
        );
        storage.set_session_effort(&s.id, None)?;
        assert_eq!(storage.list_sessions(&project.id)?[0].effort, None);
        // Re-running the migration on an up-to-date database is a no-op.
        storage.migrate()?;
        Ok(())
    }

    #[test]
    fn session_approval_mode_round_trips() -> Result<()> {
        let storage = Storage::open_in_memory()?;
        let ws = storage.ensure_default_workspace()?;
        let project = storage.ensure_project_for_path(&ws.id, &PathBuf::from("/tmp/same-m"))?;
        let s = storage.create_session(&project.id, None, "t", "claude", None)?;
        assert_eq!(s.approval_mode, None);
        storage.set_session_approval_mode(&s.id, Some(ApprovalMode::Plan))?;
        assert_eq!(
            storage.get_session(&s.id)?.unwrap().approval_mode,
            Some(ApprovalMode::Plan)
        );
        storage.set_session_approval_mode(&s.id, None)?;
        assert_eq!(storage.list_sessions(&project.id)?[0].approval_mode, None);
        Ok(())
    }

    #[test]
    fn test_storage_hierarchy_crud() -> Result<()> {
        let storage = Storage::open_in_memory()?;
        let ws = storage.ensure_default_workspace()?;
        let project = storage.ensure_project_for_path(&ws.id, &PathBuf::from("/tmp/same-test"))?;
        assert_eq!(project.name, "same-test");

        let quest = storage.create_quest(&project.id, "Build MVP", "Self-host SAME")?;
        let quests = storage.list_quests(&project.id)?;
        assert_eq!(quests.len(), 1);
        assert_eq!(quests[0].id, quest.id);

        let session = storage.create_session(
            &project.id,
            Some(&quest.id),
            "Initial session",
            "opencode",
            None,
        )?;
        let msg = Message {
            id: Uuid::new_v4().to_string(),
            session_id: session.id.clone(),
            role: MessageRole::User,
            content: "Hello SAME".to_string(),
            thinking: None,
            tool_calls: vec![],
            provider_id: Some("opencode".to_string()),
            model: None,
            created_at: Utc::now(),
        };
        storage.append_message(&msg)?;

        let loaded = storage.list_messages(&session.id)?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "Hello SAME");
        Ok(())
    }

    #[test]
    fn test_reset_active_sessions_and_delete_quest() -> Result<()> {
        let storage = Storage::open_in_memory()?;
        let ws = storage.ensure_default_workspace()?;
        let project = storage.ensure_project_for_path(&ws.id, &PathBuf::from("/tmp/same-reset"))?;
        let quest = storage.create_quest(&project.id, "Q", "")?;
        let s1 = storage.create_session(&project.id, Some(&quest.id), "a", "opencode", None)?;
        let s2 = storage.create_session(&project.id, None, "b", "opencode", None)?;
        storage.update_session_meta(
            &s1.id,
            None,
            None,
            None,
            None,
            Some(SessionStatus::Running),
        )?;
        storage.update_session_meta(
            &s2.id,
            None,
            None,
            None,
            None,
            Some(SessionStatus::Completed),
        )?;

        assert_eq!(storage.reset_active_sessions()?, 1);
        let s1 = storage.get_session(&s1.id)?.unwrap();
        assert_eq!(s1.status, SessionStatus::Interrupted);
        let s2 = storage.get_session(&s2.id)?.unwrap();
        assert_eq!(s2.status, SessionStatus::Completed);

        storage.delete_quest(&quest.id)?;
        assert!(storage.list_quests(&project.id)?.is_empty());
        // The session survives with its quest link cleared.
        assert_eq!(storage.get_session(&s1.id)?.unwrap().quest_id, None);
        Ok(())
    }
}
