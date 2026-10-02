use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use tower_http::services::ServeDir;

// ---------------------------------------------------------------- 状态与错误

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
}

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        eprintln!("伺服器錯誤：{:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>500</h1><p>伺服器內部錯誤，請看終端機訊息。</p>".to_string()),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            eprintln!("模板渲染失敗：{err}");
            (StatusCode::INTERNAL_SERVER_ERROR, "模板渲染失敗").into_response()
        }
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html("<h1>404</h1><p>找不到這個詞本。<a href=\"/\">回到首頁</a></p>".to_string()),
    )
        .into_response()
}

// ---------------------------------------------------------------- 資料庫

fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS notebooks (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT    NOT NULL,
            created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS characters (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            notebook_id INTEGER NOT NULL REFERENCES notebooks(id) ON DELETE CASCADE,
            character   TEXT    NOT NULL,
            definition  TEXT,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE (notebook_id, character)
        );

        CREATE INDEX IF NOT EXISTS idx_characters_notebook
            ON characters(notebook_id);
        "#,
    )
}

/// 判斷是否為漢字（含擴充 A/B 與相容表意文字）。
fn is_han(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF      // CJK 擴充 A
        | 0x4E00..=0x9FFF    // CJK 基本區
        | 0xF900..=0xFAFF    // 相容表意文字
        | 0x20000..=0x2FA1F  // 擴充 B ~ F
    )
}

/// 把輸入字串拆成單字逐個寫入，重複與非漢字自動忽略。
/// 回傳實際新增的數量。
fn insert_characters(db: &Connection, notebook_id: i64, input: &str) -> rusqlite::Result<usize> {
    let mut stmt = db.prepare(
        "INSERT OR IGNORE INTO characters (notebook_id, character) VALUES (?1, ?2)",
    )?;

    let mut seen = HashSet::new();
    let mut added = 0usize;

    for ch in input.chars() {
        if ch.is_whitespace() || !is_han(ch) || !seen.insert(ch) {
            continue;
        }
        added += stmt.execute(params![notebook_id, ch.to_string()])?;
    }

    Ok(added)
}

// ---------------------------------------------------------------- 模板資料

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    notebooks: Vec<NotebookSummary>,
    error: Option<String>,
}

struct NotebookSummary {
    id: i64,
    name: String,
    char_count: i64,
}

#[derive(Template)]
#[template(path = "notebook.html")]
struct NotebookTemplate {
    notebook: NotebookView,
    characters: Vec<CharacterView>,
    error: Option<String>,
}

struct NotebookView {
    id: i64,
    name: String,
}

struct CharacterView {
    id: i64,
    character: String,
    /// 目前一律為 None。之後接上字典查詢後，把結果寫進這一欄即可。
    definition: Option<String>,
}

// ---------------------------------------------------------------- 表單 / 查詢參數

#[derive(Debug, Default, Deserialize)]
struct Flash {
    #[serde(default)]
    err: Option<String>,
}

fn error_message(code: &str) -> Option<String> {
    match code {
        "empty" => Some("請先輸入至少一個字。".to_string()),
        "dup" => Some("沒有加入新字——可能已經在詞本裡，或不是漢字。".to_string()),
        "name" => Some("詞本名稱不能是空的。".to_string()),
        _ => None,
    }
}

#[derive(Deserialize)]
struct NewNotebookForm {
    name: String,
}

#[derive(Deserialize)]
struct NewCharacterForm {
    character: String,
}

// ---------------------------------------------------------------- Handlers

async fn index(
    State(state): State<AppState>,
    Query(flash): Query<Flash>,
) -> Result<Response, AppError> {
    let notebooks = {
        let db = state.db.lock().unwrap();
        let mut stmt = db.prepare(
            "SELECT n.id, n.name, COUNT(c.id)
               FROM notebooks n
               LEFT JOIN characters c ON c.notebook_id = n.id
              GROUP BY n.id, n.name
              ORDER BY n.id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(NotebookSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                char_count: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let error = flash.err.as_deref().and_then(error_message);

    Ok(render(IndexTemplate { notebooks, error }))
}

async fn create_notebook(
    State(state): State<AppState>,
    Form(form): Form<NewNotebookForm>,
) -> Result<Response, AppError> {
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Ok(Redirect::to("/?err=name").into_response());
    }

    let id = {
        let db = state.db.lock().unwrap();
        db.execute("INSERT INTO notebooks (name) VALUES (?1)", params![name])?;
        db.last_insert_rowid()
    };

    Ok(Redirect::to(&format!("/notebooks/{id}")).into_response())
}

async fn show_notebook(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(flash): Query<Flash>,
) -> Result<Response, AppError> {
    let (notebook, characters) = {
        let db = state.db.lock().unwrap();

        let notebook = db
            .query_row(
                "SELECT id, name FROM notebooks WHERE id = ?1",
                params![id],
                |row| {
                    Ok(NotebookView {
                        id: row.get(0)?,
                        name: row.get(1)?,
                    })
                },
            )
            .optional()?;

        let Some(notebook) = notebook else {
            return Ok(not_found());
        };

        let mut stmt = db.prepare(
            "SELECT id, character, definition
               FROM characters
              WHERE notebook_id = ?1
              ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![id], |row| {
            Ok(CharacterView {
                id: row.get(0)?,
                character: row.get(1)?,
                definition: row.get(2)?,
            })
        })?;
        let characters = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        (notebook, characters)
    };

    let error = flash.err.as_deref().and_then(error_message);

    Ok(render(NotebookTemplate {
        notebook,
        characters,
        error,
    }))
}

async fn delete_notebook(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    {
        let db = state.db.lock().unwrap();
        db.execute("DELETE FROM characters WHERE notebook_id = ?1", params![id])?;
        db.execute("DELETE FROM notebooks WHERE id = ?1", params![id])?;
    }
    Ok(Redirect::to("/").into_response())
}

async fn add_characters(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<NewCharacterForm>,
) -> Result<Response, AppError> {
    if form.character.trim().is_empty() {
        return Ok(Redirect::to(&format!("/notebooks/{id}?err=empty")).into_response());
    }

    let added = {
        let db = state.db.lock().unwrap();
        insert_characters(&db, id, &form.character)?
    };

    let suffix = if added == 0 { "?err=dup" } else { "" };
    Ok(Redirect::to(&format!("/notebooks/{id}{suffix}")).into_response())
}

async fn delete_character(
    State(state): State<AppState>,
    Path((notebook_id, character_id)): Path<(i64, i64)>,
) -> Result<Response, AppError> {
    {
        let db = state.db.lock().unwrap();
        db.execute(
            "DELETE FROM characters WHERE id = ?1 AND notebook_id = ?2",
            params![character_id, notebook_id],
        )?;
    }
    Ok(Redirect::to(&format!("/notebooks/{notebook_id}")).into_response())
}

// ---------------------------------------------------------------- main

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_path = std::env::var("HANZI_DB").unwrap_or_else(|_| "db/hanzi.db".to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());

    let conn = Connection::open(&db_path)
        .with_context(|| format!("無法開啟資料庫 {db_path}"))?;
    init_db(&conn).context("初始化資料表失敗")?;

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/notebooks", post(create_notebook))
        .route("/notebooks/{id}", get(show_notebook))
        .route("/notebooks/{id}/delete", post(delete_notebook))
        .route("/notebooks/{id}/characters", post(add_characters))
        .route(
            "/notebooks/{id}/characters/{cid}/delete",
            post(delete_character),
        )
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("繁字詞本已啟動 → http://{addr}");
    axum::serve(listener, app).await?;

    Ok(())
}