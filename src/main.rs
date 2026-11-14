use std::process::Command;
use glob::glob;
use std::fs;
use std::io::{self, Write};

const MAX_CHUNK_CHARS: usize = 12_000;
const DEFAULT_MODEL: &str = "haiku";
const API_BASE: &str = "https://api.anthropic.com/v1";
const FAMILIES: [&str; 4] = ["haiku", "sonnet", "opus", "fable"];

struct Ctx {
    api_key: String,
    model: String,
    supports_effort: bool,
}

struct ModelInfo {
    id: String,
    created_at: String,
    supports_low_effort: bool,
}

impl ModelInfo {
    fn from_json(m: &serde_json::Value) -> Option<Self> {
        Some(ModelInfo {
            id: m["id"].as_str()?.to_string(),
            created_at: m["created_at"].as_str().unwrap_or("").to_string(),
            supports_low_effort: m["capabilities"]["effort"]["low"]["supported"]
                .as_bool()
                .unwrap_or(false),
        })
    }
}

fn config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".config/auto_push/config")
}

fn load_saved_model() -> Option<String> {
    let content = fs::read_to_string(config_path()).ok()?;
    content.lines().find_map(|line| {
        let value = line.trim().strip_prefix("model=")?.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

fn save_model(model: &str) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("Failed to create config directory");
    }
    fs::write(&path, format!("model={model}\n")).expect("Failed to write config file");
    println!("✓ Default model set to `{model}` (saved in {})", path.display());
}

async fn list_models(api_key: &str) -> Result<Vec<ModelInfo>, String> {
    let resp = reqwest::Client::new()
        .get(format!("{API_BASE}/models?limit=1000"))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let res: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let data = res["data"]
        .as_array()
        .ok_or_else(|| format!("unexpected response: {res}"))?;
    Ok(data.iter().filter_map(ModelInfo::from_json).collect())
}

/// Newest model whose id starts with `claude-<family>` (e.g. "haiku" -> claude-haiku-5-5).
fn newest_in_family<'a>(models: &'a [ModelInfo], family: &str) -> Option<&'a ModelInfo> {
    let prefix = format!("claude-{family}-");
    models
        .iter()
        .filter(|m| m.id.starts_with(&prefix))
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
}

/// Accepts an alias ("haiku", "sonnet", "opus", "fable") or a full model id.
/// Aliases always resolve to the newest model of that family, so new releases are picked up automatically.
async fn resolve_model(wanted: &str, api_key: &str) -> (String, bool) {
    let wanted = wanted.trim().to_lowercase();
    let models = match list_models(api_key).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Warning: could not list models ({e}), using `{wanted}` as-is.");
            let fallback = match wanted.as_str() {
                "haiku" => "claude-haiku-5-5",
                "sonnet" => "claude-sonnet-5-5",
                "opus" => "claude-opus-5-5",
                "fable" => "claude-fable-5-2",
                other => other,
            };
            return (fallback.to_string(), false);
        }
    };

    if let Some(m) = models.iter().find(|m| m.id == wanted) {
        return (m.id.clone(), m.supports_low_effort);
    }
    let family = wanted.strip_prefix("claude-").unwrap_or(&wanted);
    if let Some(m) = newest_in_family(&models, family) {
        return (m.id.clone(), m.supports_low_effort);
    }

    eprintln!("Unknown model `{wanted}`. Available models:");
    for m in &models {
        eprintln!("  {}", m.id);
    }
    std::process::exit(1);
}

async fn pick_model_interactively(api_key: &str) {
    let models = list_models(api_key).await.unwrap_or_else(|e| {
        eprintln!("Could not list models: {e}");
        std::process::exit(1);
    });

    let mut choices: Vec<(String, String)> = Vec::new();
    for family in FAMILIES {
        if let Some(m) = newest_in_family(&models, family) {
            choices.push((family.to_string(), format!("{family:<8} → always the newest {family} (today: {})", m.id)));
        }
    }
    for m in &models {
        choices.push((m.id.clone(), m.id.clone()));
    }

    let current = load_saved_model().unwrap_or_else(|| DEFAULT_MODEL.to_string());
    println!("Current default: {current}\n");
    for (i, (_, label)) in choices.iter().enumerate() {
        println!("  {:>2}) {label}", i + 1);
    }
    print!("\nChoose a default model [1-{}]: ", choices.len());
    io::stdout().flush().unwrap();

    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    match input.trim().parse::<usize>() {
        Ok(n) if (1..=choices.len()).contains(&n) => save_model(&choices[n - 1].0),
        _ => {
            eprintln!("Invalid choice, nothing changed.");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!(
        "auto_push — stage, generate a commit message with Claude, commit and push.

Usage:
  auto_push                        Use the default model (currently: {})
  auto_push -model <model>         Use <model> for this run only
  auto_push -model <model> --save  Make <model> the default (no commit)
  auto_push -model                 Pick the default model from a menu (no commit)

<model> is an alias (haiku, sonnet, opus, fable) that always points to the
newest model of that family, or an exact id like claude-sonnet-5-5.",
        load_saved_model().unwrap_or_else(|| DEFAULT_MODEL.to_string())
    );
}

async fn call_claude(system: &str, user_message: &str, max_tokens: u32, ctx: &Ctx) -> String {
    let mut body = serde_json::json!({
        "model": ctx.model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": [{ "role": "user", "content": user_message }]
    });
    // Commit messages are simple: keep thinking short on models that support effort.
    if ctx.supports_effort {
        body["output_config"] = serde_json::json!({ "effort": "low" });
    }
    let api_key = ctx.api_key.as_str();

    let client = reqwest::Client::new();
    let max_retries = 3;
    let mut delay_secs = 10;

    for attempt in 0..=max_retries {
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .unwrap();

        let status = resp.status();
        let res: serde_json::Value = resp.json().await.unwrap();

        if res["stop_reason"].as_str() == Some("refusal") {
            eprintln!("The model declined this request: {}", res["stop_details"]);
            std::process::exit(1);
        }

        // Newer models may return thinking blocks before the text block.
        let text = res["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
            .and_then(|b| b["text"].as_str());
        if let Some(text) = text {
            return text.trim().to_string();
        }

        let is_rate_limit = status == 429
            || res["error"]["type"].as_str() == Some("rate_limit_error");

        if is_rate_limit && attempt < max_retries {
            eprintln!(
                "Rate limited, retrying in {}s... (attempt {}/{})",
                delay_secs,
                attempt + 1,
                max_retries
            );
            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            delay_secs *= 2;
            continue;
        }

        eprintln!("API error: {}", res);
        std::process::exit(1);
    }

    unreachable!()
}

fn split_diff_into_chunks(diff: &str) -> Vec<String> {
    if diff.len() <= MAX_CHUNK_CHARS {
        return vec![diff.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current_chunk = String::new();

    for line in diff.lines() {
        if current_chunk.len() + line.len() + 1 > MAX_CHUNK_CHARS && !current_chunk.is_empty() {
            chunks.push(current_chunk);
            current_chunk = String::new();
        }
        current_chunk.push_str(line);
        current_chunk.push('\n');
    }
    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
}

async fn summarize_chunk(chunk: &str, chunk_index: usize, total_chunks: usize, ctx: &Ctx) -> String {
    let system = "You summarize git diffs. Be concise but preserve all important details: files changed, what was added/removed/modified, and why.";
    let prompt = format!(
        "Summarize this git diff (part {}/{}):\n\n{}",
        chunk_index + 1,
        total_chunks,
        chunk
    );
    println!("  Summarizing chunk {}/{}...", chunk_index + 1, total_chunks);
    call_claude(system, &prompt, 4096, ctx).await
}

async fn generate_commit_message(context: &str, ctx: &Ctx) -> String {
    let system = "You generate git commit messages. Output ONLY the commit message. No markdown, no backticks.";
    let prompt = format!(
r#"You are a Git commit message generator.

Analyze the following changes and generate a commit message.

Rules:
- First line: a short commit title using Conventional Commits format (feat:, fix:, refactor:, docs:, chore:, test:, style:, perf:)
- Second line: empty
- Third line onwards: a concise description of what changed and why (2-5 lines max)
- Write in English
- Be specific, not vague (no "updated files" or "various changes")
- Focus on the WHY and WHAT, not the HOW

Output format (strictly follow this, no extra text):
<title>
<empty line>
<description>

Example output:
feat: add user authentication via OAuth2

Implement Google OAuth2 login flow with token refresh.
Add session middleware and protect /dashboard routes.
Store refresh tokens in encrypted cookie.

--- CHANGES ---
{context}"#
    );
    call_claude(system, &prompt, 4096, ctx).await
}

fn get_api_key() -> String {
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            print!("ANTHROPIC_API_KEY not found. Enter your API key: ");
            io::stdout().flush().unwrap();

            let mut key = String::new();
            io::stdin().read_line(&mut key).unwrap();
            let key = key.trim().to_string();

            let home = std::env::var("HOME").unwrap();
            let shell_rc = if std::path::Path::new(&format!("{home}/.zshrc")).exists() {
                format!("{home}/.zshrc")
            } else {
                format!("{home}/.bashrc")
            };

            let rc_content = std::fs::read_to_string(&shell_rc).unwrap_or_default();
            if !rc_content.contains("ANTHROPIC_API_KEY") {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&shell_rc)
                    .unwrap();
                writeln!(file, "\nexport ANTHROPIC_API_KEY=\"{key}\"").unwrap();
            }

            println!("✓ Key saved to {shell_rc}");
            println!("  Run `source {shell_rc}` or restart your terminal for future sessions.");

            unsafe { std::env::set_var("ANTHROPIC_API_KEY", &key) };

            key
        }
    }
}

fn get_the_diff_for_github() -> Result<String, String> {
	let output = Command
		::new("git")
		.args(&["diff", "--staged"])
		.output()
		.map_err(|e| format!("Failed to execute git command: {}", e))?;
	if !output.status.success()
	{
		return Err(format!("Git command failed with status: {}", output.status));
	}
	let diff = String::from_utf8(output.stdout)
		.map_err(|e| format!("Failed to parse git output: {}", e))?;
	if diff.is_empty()
	{
		return Err("No differences found between the last two commits.".to_string());
	}
	Ok(diff)
}

const MAX_README_CHARS: usize = 2_000;

fn get_all_the_readmes() -> Result<String, String> {
	let mut readmes = String::new();
	let mut total_chars = 0;
	for entry in glob("**/README*.md").unwrap().flatten() {
		let path = entry;
		let content = fs::read_to_string(&path)
			.map_err(|e| format!("Failed to read file {}: {}", path.display(), e))?;
		readmes.push_str("readme name is: \t");
		readmes.push_str(path.file_name().unwrap().to_string_lossy().as_ref());
		readmes.push_str("\n\n");
		readmes.push_str("content is: \n");
		let remaining = MAX_README_CHARS.saturating_sub(total_chars);
		if remaining == 0 {
			readmes.push_str("[truncated]\n\n");
			break;
		}
		let truncated = &content[..content.len().min(remaining)];
		readmes.push_str(truncated);
		if content.len() > remaining {
			readmes.push_str("\n[truncated]");
		}
		readmes.push_str("\n\n");
		total_chars += truncated.len();
	}
	Ok(readmes)
}

fn cut_the_prompt(prompt: &str) -> (String, String) {
	let mut lines = prompt.lines();
	let title = lines.next().unwrap_or("").to_string();
	let remaining: Vec<&str> = lines.collect();
	let body = if remaining.first() == Some(&"") {
		remaining[1..].join("\n")
	} else {
		remaining.join("\n")
	};
	(title, body)
}

#[tokio::main]
async fn main() {
	let args: Vec<String> = std::env::args().skip(1).collect();
	if args.iter().any(|a| a == "-h" || a == "--help") {
		print_help();
		return;
	}
	let save = args.iter().any(|a| a == "--save");
	let model_flag = args.iter().position(|a| a == "-model" || a == "--model" || a == "-m");
	let model_arg = model_flag.map(|i| args.get(i + 1).filter(|v| !v.starts_with('-')).cloned());

	if let Some(None) = model_arg {
		let api_key = get_api_key();
		pick_model_interactively(&api_key).await;
		return;
	}
	if let Some(Some(model)) = &model_arg
		&& save
	{
		save_model(model);
		return;
	}

	Command::new("git")
		.args(&["add", "."])
		.output()
		.expect("Failed to execute git add command");

	let diff = match get_the_diff_for_github() {
		Ok(d) => d,
		Err(e) => {
			eprintln!("Error: {}", e);
			return;
		}
	};

	let readmes = get_all_the_readmes().unwrap_or_default();

	let api_key = get_api_key();
	let wanted = model_arg
		.flatten()
		.or_else(load_saved_model)
		.unwrap_or_else(|| DEFAULT_MODEL.to_string());
	let (model, supports_effort) = resolve_model(&wanted, &api_key).await;
	println!("Using model: {model}");
	let ctx = Ctx { api_key, model, supports_effort };

	let chunks = split_diff_into_chunks(&diff);
	let context = if chunks.len() == 1 {
		println!("Generating commit message...");
		let mut ctx = String::new();
		ctx.push_str(&diff);
		if !readmes.is_empty() {
			ctx.push_str("\n\n--- README CONTEXT ---\n");
			ctx.push_str(&readmes);
		}
		ctx
	} else {
		println!("Diff is large ({} chars), splitting into {} chunks...", diff.len(), chunks.len());
		let mut summaries = Vec::new();
		for (i, chunk) in chunks.iter().enumerate() {
			let summary = summarize_chunk(chunk, i, chunks.len(), &ctx).await;
			summaries.push(summary);
		}
		let mut ctx = String::from("Summaries of all changes:\n\n");
		for (i, summary) in summaries.iter().enumerate() {
			ctx.push_str(&format!("--- Part {}/{} ---\n{}\n\n", i + 1, summaries.len(), summary));
		}
		if !readmes.is_empty() {
			ctx.push_str("--- README CONTEXT ---\n");
			ctx.push_str(&readmes);
		}
		ctx
	};

	let prompt_result = generate_commit_message(&context, &ctx).await;

	let (title, body) = cut_the_prompt(&prompt_result);
	println!("Generated commit message:\n{}\n{}", title, body);
	let mut cmd = Command::new("git");
	cmd.args(["commit", "-m", &title]);
	if !body.is_empty() {
		cmd.args(["-m", &body]);
	}
	let commit_output = cmd.output().expect("Failed to execute git commit");
	if commit_output.status.success() {
		println!("Commit created successfully.");
		let mut push_cmd = Command::new("git");
		push_cmd.arg("push");
		let push_output = push_cmd.output().expect("Failed to execute git push");
		if push_output.status.success() {
			println!("Changes pushed successfully.");
		} else {
			let stderr = String::from_utf8_lossy(&push_output.stderr);
			eprintln!("Git push failed: {}", stderr);
		}
	} else {
		let stderr = String::from_utf8_lossy(&commit_output.stderr);
		eprintln!("Git commit failed: {}", stderr);
	}
}
