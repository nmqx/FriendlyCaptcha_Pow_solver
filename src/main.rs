use axum::{routing::post, Json, Router};
use base64::{engine::general_purpose::STANDARD, engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use blake2b_simd::Params;
use rand::{Rng, RngCore};
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio::net::TcpListener;

// Task State Management

#[derive(Clone)]
enum TaskStatus {
    Processing,
    Ready,
    Error,
}

#[derive(Clone)]
struct Task {
    status: TaskStatus,
    solution: Option<String>,
    error: Option<String>,
}

type TaskDb = Arc<RwLock<HashMap<String, Task>>>;

// API Request Models

#[derive(Deserialize)]
struct CreateTaskReq {
    puzzle: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetResultReq {
    task_id: String,
}

// Helper Functions

fn decode_b64(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD.decode(input).or_else(|_| STANDARD_NO_PAD.decode(input))
}

fn set_error(db: &TaskDb, task_id: &str, error_msg: &str) {
    let mut db_write = db.write().unwrap();
    if let Some(task) = db_write.get_mut(task_id) {
        task.status = TaskStatus::Error;
        task.error = Some(error_msg.to_string());
    }
}

// The High-Performance CPU Burner

fn solve_puzzle(task_id: String, db: TaskDb, puzzle_payload: String) {
    let start_time = Instant::now();

    let parts: Vec<&str> = puzzle_payload.split('.').collect();
    if parts.len() != 2 {
        set_error(&db, &task_id, "Invalid puzzle format");
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

    // 1. CALCULATE EXACT BROWSER TIME (WASM SPEED)
    let expected_hashes_per_puzzle = 4_294_967_296.0 / (threshold as f64);
    let total_expected_hashes = (n_puzzles as f64) * expected_hashes_per_puzzle;
    
    // WASM engine averages 18 MH/s. This drastically drops the simulated time.
    let browser_hash_rate = 18_000_000.0; 
    let base_time_s = total_expected_hashes / browser_hash_rate;

    // Add +/- 15% natural jitter
    use rand::Rng;
    let jitter_factor = rand::thread_rng().gen_range(0.85..1.15);
    let simulated_time_s = (base_time_s * jitter_factor).round() as u16;
    let fake_time_taken = simulated_time_s.max(1); // At least 1 second

    println!("[*] Task {}: Expected hashes: {}, Simulated WASM Time: {}s", task_id, total_expected_hashes.round(), fake_time_taken);

    // 2. SOLVE AT MAX SIMD CPU SPEED
    let solutions_vec: Vec<Vec<u8>> = (0..n_puzzles)
        .into_par_iter()
        .map(|p| {
            let mut buf = [0u8; 128];
            buf[..puzzle_data.len()].copy_from_slice(&puzzle_data);
            buf[120] = p as u8;

            let mut nonce: u32 = 0;
            let mut hasher_params = Params::new();
            hasher_params.hash_length(32);

            loop {
                buf[124..128].copy_from_slice(&nonce.to_le_bytes());
                let hash = hasher_params.hash(&buf);
                let hash_val = u32::from_le_bytes(hash.as_bytes()[0..4].try_into().unwrap());

                if hash_val < threshold {
                    return buf[120..128].to_vec();
                }
                nonce += 1;
            }
        })
        .collect();

    let actual_time_taken = start_time.elapsed();

    let mut flat_solutions = Vec::with_capacity(n_puzzles * 8);
    for sol in solutions_vec {
        flat_solutions.extend(sol);
    }

    // 3. TELEMETRY PING (WASM + BIG ENDIAN FIX)
    let mut diag = vec![0u8; 3];
    diag[0] = 2; // 2 = WASM Solver (Matches our 18 MH/s speed assumption)
    
    // CRITICAL: to_be_bytes() formats the time exactly how the JS DataView expects it
    diag[1..3].copy_from_slice(&fake_time_taken.to_be_bytes());

    let sol_b64 = STANDARD.encode(&flat_solutions);
    let diag_b64 = STANDARD.encode(&diag);

    let final_solution = format!("{}.{}.{}.{}", signature, b64_puzzle_data, sol_b64, diag_b64);

    // 4. HOLD AND RELEASE
    let simulated_duration = std::time::Duration::from_secs(fake_time_taken as u64);
    if simulated_duration > actual_time_taken {
        let sleep_time = simulated_duration - actual_time_taken;
        println!("[*] Task {} holding for {:?} to match WASM fake telemetry...", task_id, sleep_time);
        std::thread::sleep(sleep_time);
    }

    println!("[+] Task {} ready!", task_id);

    let mut db_write = db.write().unwrap();
    if let Some(task) = db_write.get_mut(&task_id) {
        task.status = TaskStatus::Ready;
        task.solution = Some(final_solution);
    }
}

// API Endpoints

async fn create_task(
    axum::extract::State(db): axum::extract::State<TaskDb>,
    Json(payload): Json<CreateTaskReq>,
) -> Json<serde_json::Value> {
    if payload.puzzle.is_empty() {
        return Json(serde_json::json!({
            "errorId": 1,
            "errorDescription": "Missing puzzle"
        }));
    }

    // Fast-Fail Difficulty Check
    let parts: Vec<&str> = payload.puzzle.split('.').collect();
    if parts.len() == 2 {
        if let Ok(puzzle_data) = decode_b64(parts[1]) {
            if puzzle_data.len() >= 16 {
                let threshold_byte = puzzle_data[15] as f64;
                let threshold = 2f64.powf((255.999 - threshold_byte) / 8.0) as u32;
                
                // If it's going to take longer than ~8 seconds, reject it so OB rotates proxy
                if threshold < 1000 {
                    println!("[!] Puzzle rejected upfront. Threshold {} is < 1000.", threshold);
                    return Json(serde_json::json!({
                        "errorId": 1,
                        "status": "too_long"
                    }));
                }
            }
        }
    }

    // Generate Task ID
    let mut tid_bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut tid_bytes);
    let task_id = hex::encode(tid_bytes);

    // Save initial state
    {
        let mut db_write = db.write().unwrap();
        db_write.insert(
            task_id.clone(),
            Task {
                status: TaskStatus::Processing,
                solution: None,
                error: None,
            },
        );
    }

    println!("[*] New task created: {}. Starting background SIMD solver...", task_id);

    // Spawn blocking task
    let db_clone = db.clone();
    let task_id_clone = task_id.clone();
    tokio::task::spawn_blocking(move || {
        solve_puzzle(task_id_clone, db_clone, payload.puzzle);
    });

    Json(serde_json::json!({
        "errorId": 0,
        "taskId": task_id
    }))
}

async fn get_task_result(
    axum::extract::State(db): axum::extract::State<TaskDb>,
    Json(payload): Json<GetResultReq>,
) -> Json<serde_json::Value> {
    let db_read = db.read().unwrap();
    
    match db_read.get(&payload.task_id) {
        Some(task) => match task.status {
            TaskStatus::Ready => Json(serde_json::json!({
                "errorId": 0,
                "status": "ready",
                "solution": {
                    "frc_solution": task.solution
                }
            })),
            TaskStatus::Processing => Json(serde_json::json!({
                "errorId": 0,
                "status": "processing"
            })),
            TaskStatus::Error => Json(serde_json::json!({
                "errorId": 1,
                "status": "error",
                "errorDescription": task.error
            })),
        },
        None => Json(serde_json::json!({
            "errorId": 1,
            "status": "error",
            "errorDescription": "Task not found"
        })),
    }
}

#[tokio::main]
async fn main() {
    let db: TaskDb = Arc::new(RwLock::new(HashMap::new()));

    let app = Router::new()
        .route("/createTask", post(create_task))
        .route("/getTaskResult", post(get_task_result))
        .with_state(db);

    let listener = TcpListener::bind("127.0.0.1:8989").await.unwrap();
    println!("frc solver running on http://127.0.0.1:8989");
    axum::serve(listener, app).await.unwrap();
}