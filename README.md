# FriendlyCaptcha (FRC) Solver

A standalone Rust-based solver for FriendlyCaptcha puzzles that exposes a standard HTTP API. It performs proof-of-work calculations using SIMD (`blake2b_simd`), fast-fails difficult puzzles, and spoofs execution telemetry to simulate normal browser solving speeds.

> **Note:** This is currently a proof of concept. The solver may struggle with multiple concurrent threads/tasks at this stage.

## Features
- **High-Speed Execution:** Uses SIMD and `rayon` for massive multithreading speedups.
- **WASM Time Simulation:** Calculates expected execution time of the in-browser WASM solver (~18 MH/s) and holds the solution until that time passes to spoof telemetry accurately.
- **Difficulty Threshold Fast-Fail:** Evaluates puzzle difficulty and immediately rejects it if the threshold is abnormally hard (threshold < 1000).
- **Asynchronous Task Queue:** Tasks are processed entirely in the background.

## Requirements
- Rust toolchain (cargo)

## Building & Running

Run the program via Cargo:

```sh
cd frc_solver
cargo build --release
cargo run --release
```

The server binds to `127.0.0.1:8989` by default.

## API Documentation

### Create a Task (`/createTask`)

Submits a new puzzle to be solved. If the puzzle is too difficult, it will return an immediate error, otherwise it responds with a task ID.

**Endpoint:** `POST http://127.0.0.1:8989/createTask`

**Request Body (JSON):**
```json
{
  "puzzle": "SIGNATURE.BASE64_PAYLOAD"
}
```

**Response (JSON):**
```json
{
  "errorId": 0,
  "taskId": "hex_string_task_id"
}
```
*(If puzzle rejected due to length/difficulty: `{"errorId": 1, "status": "too_long"}`)*

### Get Task Result (`/getTaskResult`)

Polls the solver for the result of a specific task. To maintain telemetry consistency, the server will intentionally hold the solution in the `Processing` state until the simulated WASM duration holds pass.

**Endpoint:** `POST http://127.0.0.1:8989/getTaskResult`

**Request Body (JSON):**
```json
{
  "task_id": "hex_string_task_id"
}
```

**Response (JSON - Expected):**
```json
{
  "errorId": 0,
  "status": "ready",
  "solution": {
    "frc_solution": "signature.b64_puzzle.b64_sol.b64_diag"
  }
}
```

**Responses for pending / errored tasks:**
- Processing: `{"errorId": 0, "status": "processing"}`
- Errored: `{"errorId": 1, "status": "error", "errorDescription": "Error message"}`
