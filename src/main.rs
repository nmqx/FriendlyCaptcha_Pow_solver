use axum::{
    routing::post,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use blake2b_simd::Params;
use rand::RngCore;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::net::TcpListener;

const THRESHOLD_MIN: u32 = 3000;
// How long (secs) before we kill a task nobody is polling
const ABANDON_SECS: u64 = 3;

// Task State Management

#[derive(Clone, Serialize)]
#[serde(rename_all = "lowercase")]
enum TaskStatus {
    Processing,
    Ready,
    Error,
    Cancelled,
}

#[derive(Clone)]
struct Task {
    status: TaskStatus,
    solution: Option<String>,
    error: Option<String>,
    // Set to true the moment getTaskResult is called for this task
    polled: Arc<AtomicBool>,
    // Used by the sweeper to know when to give up
    cancel: Arc<AtomicBool>,
    created_at: Instant,
}

type TaskDb = Arc<RwLock<HashMap<String, Task>>>;

// API Models

#[derive(Deserialize)]
struct CreateTaskReq {
    #[serde(default)]
    puzzle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activate_payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateTaskRes {
    error_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetResultReq {
    task_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetResultRes {
    error_id: i32,
    status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    solution: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_description: Option<String>,
}

// The CPU Burner Logic

fn decode_b64(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD.decode(input)
        .or_else(|_| STANDARD_NO_PAD.decode(input))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(input))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(input))
}

fn solve_puzzle(task_id: String, db: TaskDb, puzzle_payload: String, cancel: Arc<AtomicBool>) {
    let start_time = Instant::now();

    let parts: Vec<&str> = puzzle_payload.split('.').collect();
    if parts.len() != 2 {
        let msg = format!("Invalid puzzle format, got {} parts.", parts.len());
        set_error(&db, &task_id, &msg);
        return;
    }

    let signature = parts[0];
    let b64_puzzle_data = parts[1];

    let puzzle_data = match decode_b64(b64_puzzle_data) {
        Ok(data) => data,
        Err(_) => {
            set_error(&db, &task_id, "Base64 decode failed");
            return;
        }
    };

    if puzzle_data.len() < 16 {
        set_error(&db, &task_id, "Puzzle data too short");
        return;
    }

    let n_puzzles = puzzle_data[14] as usize;
    let threshold_byte = puzzle_data[15] as f64;
    let threshold = 2f64.powf((255.999 - threshold_byte) / 8.0) as u32;

    if threshold < THRESHOLD_MIN {
        let msg = format!("Refusing: threshold {} < {} (IP likely flagged)", threshold, THRESHOLD_MIN);
        set_error(&db, &task_id, &msg);
        return;
    }

    println!("[*] Task {}: {} sub-puzzles, threshold={}", task_id, n_puzzles, threshold);

    let buf_size = puzzle_data.len() + 8;
    let nonce_offset = buf_size - 8;
    let nonce_u32_offset = buf_size - 4;

    // RAYON PARALLEL ITERATOR — checks cancel flag every 50k iterations
    let solutions_vec: Vec<Option<Vec<u8>>> = (0..n_puzzles)
        .into_par_iter()
        .map(|p| {
            let mut buf = vec![0u8; buf_size];
            buf[..puzzle_data.len()].copy_from_slice(&puzzle_data);
            buf[nonce_offset] = p as u8;

            let mut nonce: u32 = 0;
            let mut hasher_params = Params::new();
            hasher_params.hash_length(32);

            loop {
                // Check cancellation every 50k iters to avoid slowing hashing
                if nonce % 50_000 == 0 && cancel.load(Ordering::Relaxed) {
                    return None;
                }

                buf[nonce_u32_offset..buf_size].copy_from_slice(&nonce.to_le_bytes());
                let hash = hasher_params.hash(&buf);
                let hash_val = u32::from_le_bytes(hash.as_bytes()[0..4].try_into().unwrap());

                if hash_val < threshold {
                    return Some(buf[nonce_offset..buf_size].to_vec());
                }
                nonce += 1;
            }
        })
        .collect();

    // If we were cancelled mid-solve, just bail out silently
    if cancel.load(Ordering::Relaxed) {
        println!("[!] Task {} was cancelled (nobody polled within {}s). Discarding.", task_id, ABANDON_SECS);
        let mut db_write = db.write().unwrap();
        if let Some(task) = db_write.get_mut(&task_id) {
            task.status = TaskStatus::Cancelled;
            task.error = Some("Abandoned: no poll received in time".to_string());
        }
        return;
    }

    let time_taken = start_time.elapsed().as_secs() as u16;

    let mut flat_solutions = Vec::with_capacity(n_puzzles * 8);
    for sol in solutions_vec {
        flat_solutions.extend(sol.unwrap());
    }

    let mut diag = vec![1u8; 3];
    diag[1..3].copy_from_slice(&time_taken.to_le_bytes());

    let sol_b64 = STANDARD.encode(&flat_solutions);
    let diag_b64 = STANDARD.encode(&diag);
    let final_solution = format!("{}.{}.{}.{}", signature, b64_puzzle_data, sol_b64, diag_b64);

    println!("[+] Task {} solved in {}s!", task_id, time_taken);

    let mut db_write = db.write().unwrap();
    if let Some(task) = db_write.get_mut(&task_id) {
        task.status = TaskStatus::Ready;
        task.solution = Some(final_solution);
    }
}

fn set_error(db: &TaskDb, task_id: &str, error_msg: &str) {
    let mut db_write = db.write().unwrap();
    if let Some(task) = db_write.get_mut(task_id) {
        task.status = TaskStatus::Error;
        task.error = Some(error_msg.to_string());
    }
}

// Endpoints

async fn create_task(
    axum::extract::State(db): axum::extract::State<TaskDb>,
    Json(payload): Json<CreateTaskReq>,
) -> Json<CreateTaskRes> {
    if payload.puzzle.is_none() && payload.activate_payload.is_none() {
        return Json(CreateTaskRes {
            error_id: 1,
            task_id: None,
            error_description: Some("Missing puzzle or activate_payload".to_string()),
        });
    }

    let mut tid_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut tid_bytes);
    let task_id = hex::encode(tid_bytes);

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let polled_flag = Arc::new(AtomicBool::new(false));

    {
        let mut db_write = db.write().unwrap();
        db_write.insert(
            task_id.clone(),
            Task {
                status: TaskStatus::Processing,
                solution: None,
                error: None,
                polled: polled_flag.clone(),
                cancel: cancel_flag.clone(),
                created_at: Instant::now(),
            },
        );
    }

    println!("[*] New task created: {}", task_id);

    let db_clone = db.clone();
    let task_id_clone = task_id.clone();
    let cancel_clone = cancel_flag.clone();

    tokio::spawn(async move {
        let puzzle_payload = if let Some(activate_data) = payload.activate_payload {
            let mut builder = reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36");

            if let Some(proxy_str) = payload.proxy {
                if let Ok(p) = reqwest::Proxy::all(&proxy_str) {
                    builder = builder.proxy(p);
                }
            }

            let client = match builder.build() {
                Ok(c) => c,
                Err(e) => {
                    set_error(&db_clone, &task_id_clone, &format!("HTTP client build failed: {}", e));
                    return;
                }
            };

            let res = match client.post("https://global.frcapi.com/api/v2/captcha/activate")
                .header("origin", "https://global.frcapi.com")
                .header("frc-agent-id", "a_MsItqi4Jw9rn")
                .header("frc-sdk", "friendly-captcha-sdk@0.1.31")
                .json(&activate_data)
                .send()
                .await {
                    Ok(r) => r,
                    Err(e) => {
                        set_error(&db_clone, &task_id_clone, &format!("Network fetch failed: {}", e));
                        return;
                    }
                };

            if !res.status().is_success() {
                set_error(&db_clone, &task_id_clone, &format!("Activate HTTP {}", res.status()));
                return;
            }

            let body: serde_json::Value = match res.json().await {
                Ok(b) => b,
                Err(e) => {
                    set_error(&db_clone, &task_id_clone, &format!("Parse failed: {}", e));
                    return;
                }
            };

            let ctx_opt = body["data"]["solve_context"].as_str()
                .or_else(|| body["solve_context"].as_str())
                .or_else(|| body["puzzle"].as_str());

            match ctx_opt {
                Some(ctx) => ctx.to_string(),
                None => {
                    set_error(&db_clone, &task_id_clone, &format!("No solve_context in response: {}", body));
                    return;
                }
            }
        } else {
            payload.puzzle.unwrap()
        };

        tokio::task::spawn_blocking(move || {
            solve_puzzle(task_id_clone, db_clone, puzzle_payload, cancel_clone);
        });
    });

    Json(CreateTaskRes {
        error_id: 0,
        task_id: Some(task_id),
        error_description: None,
    })
}

async fn get_task_result(
    axum::extract::State(db): axum::extract::State<TaskDb>,
    Json(payload): Json<GetResultReq>,
) -> Json<GetResultRes> {
    let db_read = db.read().unwrap();

    match db_read.get(&payload.task_id) {
        Some(task) => {
            // Mark as polled so the sweeper doesn't kill it
            task.polled.store(true, Ordering::Relaxed);

            let mut solution_map = None;
            if let Some(sol) = &task.solution {
                let mut map = HashMap::new();
                map.insert("frc_solution".to_string(), sol.clone());
                solution_map = Some(map);
            }

            Json(GetResultRes {
                error_id: if task.error.is_some() { 1 } else { 0 },
                status: task.status.clone(),
                solution: solution_map,
                error_description: task.error.clone(),
            })
        }
        None => Json(GetResultRes {
            error_id: 1,
            status: TaskStatus::Error,
            solution: None,
            error_description: Some("Task not found".to_string()),
        }),
    }
}

// Sweeper: kills tasks that nobody polls within ABANDON_SECS
async fn run_sweeper(db: TaskDb) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let db_read = db.read().unwrap();
        for (tid, task) in db_read.iter() {
            if let TaskStatus::Processing = task.status {
                let not_polled = !task.polled.load(Ordering::Relaxed);
                let timed_out = task.created_at.elapsed().as_secs() >= ABANDON_SECS;
                if not_polled && timed_out {
                    println!("[!] Sweeper: cancelling task {} (no poll in {}s)", tid, ABANDON_SECS);
                    task.cancel.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let db: TaskDb = Arc::new(RwLock::new(HashMap::new()));

    // Start background sweeper
    tokio::spawn(run_sweeper(db.clone()));

    let app = Router::new()
        .route("/createTask", post(create_task))
        .route("/getTaskResult", post(get_task_result))
        .with_state(db);

    let listener = TcpListener::bind("127.0.0.1:8989").await.unwrap();
    println!(" Solver running on http://127.0.0.1:8989");
    axum::serve(listener, app).await.unwrap();
}