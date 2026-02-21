use std::collections::BTreeMap;
use std::path::Path;

use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct ExecuteResponse {
    result: serde_json::Value,
    tool_calls: usize,
    elapsed_ms: u128,
    stdout: Vec<String>,
}

#[tokio::test]
async fn gambi_execute_runs_python_and_bridges_namespaced_tool_calls() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture(BTreeMap::new()).await?;

    let response = execute(
        &client,
        r#"
payload = fixture.fixture_echo(message="hello", count=2)
return {"message": payload["echo"]["message"], "count": payload["echo"]["count"]}
"#,
    )
    .await?;

    assert_eq!(response.result["message"], "hello");
    assert_eq!(response.result["count"], 2);
    assert_eq!(response.tool_calls, 1);
    assert!(response.elapsed_ms > 0);
    assert!(response.stdout.is_empty());

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_execute_supports_zero_argument_tool_calls() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture(BTreeMap::new()).await?;

    let response = execute(
        &client,
        r#"
payload = fixture.fixture_echo()
return {"echo": payload["echo"]}
"#,
    )
    .await?;

    assert_eq!(response.result["echo"], serde_json::json!({}));
    assert_eq!(response.tool_calls, 1);

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_execute_supports_multiple_pause_resume_tool_calls() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture(BTreeMap::new()).await?;

    let response = execute(
        &client,
        r#"
first = fixture.fixture_echo(step=1)
second = fixture.fixture_echo(step=2)
return {"steps": [first["echo"]["step"], second["echo"]["step"]]}
"#,
    )
    .await?;

    assert_eq!(response.result["steps"], serde_json::json!([1, 2]));
    assert_eq!(response.tool_calls, 2);

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_execute_rejects_positional_tool_arguments_with_clear_error() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture(BTreeMap::new()).await?;

    let err = execute(
        &client,
        r#"
payload = fixture.fixture_echo("hello")
return payload
"#,
    )
    .await
    .expect_err("positional arguments should be rejected");

    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("keyword arguments only"),
        "expected keyword-only guidance, got: {err:#}"
    );

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_execute_treats_upstream_tool_is_error_as_execution_failure() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture(BTreeMap::new()).await?;

    let err = execute(
        &client,
        r#"
return fixture.fixture_error()
"#,
    )
    .await
    .expect_err("upstream tool is_error should fail gambi_execute");

    let message = err.to_string();
    assert!(
        message.contains("upstream tool 'fixture:fixture_error' returned an error"),
        "expected upstream error propagation, got: {err:#}"
    );

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_execute_enforces_wall_timeout() -> anyhow::Result<()> {
    let mut env = BTreeMap::new();
    env.insert("GAMBI_EXEC_MAX_WALL_MS".to_string(), "250".to_string());
    let (client, _temp) = spawn_gambi_with_fixture(env).await?;

    let err = execute(
        &client,
        r#"
while True:
    pass
return {"ok": True}
"#,
    )
    .await
    .expect_err("execution should time out");

    assert!(
        err.to_string().to_lowercase().contains("timed out"),
        "expected timeout error, got: {err:#}"
    );

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_execute_enforces_memory_limit() -> anyhow::Result<()> {
    let mut env = BTreeMap::new();
    env.insert(
        "GAMBI_EXEC_MAX_MEM_BYTES".to_string(),
        "33554432".to_string(),
    );
    env.insert(
        "GAMBI_EXEC_MAX_ALLOC_BYTES".to_string(),
        "16777216".to_string(),
    );
    let (client, _temp) = spawn_gambi_with_fixture(env).await?;

    let err = execute(
        &client,
        r#"
blob = "x" * (128 * 1024 * 1024)
return {"len": len(blob)}
"#,
    )
    .await
    .expect_err("memory overrun must fail");

    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("memory")
            || message.contains("killed")
            || message.contains("failed")
            || message.contains("runtime"),
        "expected memory-related execution failure, got: {err:#}"
    );

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_execute_enforces_stdout_capture_cap() -> anyhow::Result<()> {
    let mut env = BTreeMap::new();
    env.insert(
        "GAMBI_EXEC_MAX_STDOUT_BYTES".to_string(),
        "1024".to_string(),
    );
    let (client, _temp) = spawn_gambi_with_fixture(env).await?;

    let err = execute(
        &client,
        r#"
for _ in range(256):
    print("x" * 64)
return {"ok": True}
"#,
    )
    .await
    .expect_err("stdout overrun must fail");

    assert!(
        err.to_string().to_lowercase().contains("stdout"),
        "expected stdout-related failure, got: {err:#}"
    );

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_execute_rejects_unsupported_os_access() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture(BTreeMap::new()).await?;

    let err = execute(
        &client,
        r#"
handle = open("/tmp/gambi-should-not-write", "w")
handle.write("nope")
handle.close()
return {"ok": True}
"#,
    )
    .await
    .expect_err("unsupported OS access must fail");

    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("unsupported os call")
            || message.contains("os call")
            || message.contains("open"),
        "expected OS access rejection, got: {err:#}"
    );

    let _ = client.cancel().await;
    Ok(())
}

#[tokio::test]
async fn gambi_execute_supports_json_loads_and_dumps() -> anyhow::Result<()> {
    let (client, _temp) = spawn_gambi_with_fixture(BTreeMap::new()).await?;

    // json_loads parses a JSON string into a dict
    let response = execute(
        &client,
        r#"
parsed = json_loads('{"a": 1, "b": [2, 3]}')
return parsed
"#,
    )
    .await?;
    assert_eq!(response.result["a"], 1);
    assert_eq!(response.result["b"], serde_json::json!([2, 3]));

    // json_dumps serializes a dict to a JSON string
    let response = execute(
        &client,
        r#"
dumped = json_dumps({"key": [1, 2, 3]})
return {"json_string": dumped}
"#,
    )
    .await?;
    let json_string = response.result["json_string"]
        .as_str()
        .expect("json_dumps should return a string");
    let reparsed: serde_json::Value = serde_json::from_str(json_string)?;
    assert_eq!(reparsed["key"], serde_json::json!([1, 2, 3]));

    // round-trip: json_loads(json_dumps(value)) == value
    let response = execute(
        &client,
        r#"
original = {"x": 42, "y": "hello"}
round_tripped = json_loads(json_dumps(original))
return {"match": round_tripped == original, "value": round_tripped}
"#,
    )
    .await?;
    assert_eq!(response.result["match"], true);
    assert_eq!(response.result["value"]["x"], 42);
    assert_eq!(response.result["value"]["y"], "hello");

    // json_loads with invalid JSON fails with a clear error
    let err = execute(
        &client,
        r#"
return json_loads("not json")
"#,
    )
    .await
    .expect_err("invalid JSON should fail");
    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("json_loads failed"),
        "expected json_loads error, got: {err:#}"
    );

    // json_loads handles top-level arrays, booleans, nulls, numbers
    let response = execute(
        &client,
        r#"
arr = json_loads('[1, "two", null, true]')
return {"arr": arr}
"#,
    )
    .await?;
    assert_eq!(
        response.result["arr"],
        serde_json::json!([1, "two", null, true])
    );

    let response = execute(&client, "return json_loads(\"true\")").await?;
    assert_eq!(response.result, true);

    let response = execute(&client, "return json_loads(\"null\")").await?;
    assert_eq!(response.result, serde_json::Value::Null);

    let response = execute(&client, "return json_loads(\"42\")").await?;
    assert_eq!(response.result, 42);

    // json_dumps handles None, booleans, lists, nested dicts
    let response = execute(
        &client,
        r#"
return {
    "none": json_dumps(None),
    "bool": json_dumps(True),
    "list": json_dumps([1, "a", None]),
    "nested": json_dumps({"a": {"b": [1, 2]}}),
}
"#,
    )
    .await?;
    assert_eq!(response.result["none"].as_str().unwrap(), "null");
    assert_eq!(response.result["bool"].as_str().unwrap(), "true");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(response.result["list"].as_str().unwrap())?,
        serde_json::json!([1, "a", null])
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(response.result["nested"].as_str().unwrap())?,
        serde_json::json!({"a": {"b": [1, 2]}})
    );

    // json_loads with unicode
    let response = execute(
        &client,
        r#"
return json_loads('{"emoji": "\u2764", "text": "caf\u00e9"}')
"#,
    )
    .await?;
    assert_eq!(response.result["emoji"], "\u{2764}");
    assert_eq!(response.result["text"], "caf\u{e9}");

    // json_dumps preserves unicode round-trip
    let response = execute(
        &client,
        r#"
original = json_loads('{"emoji": "\u2764"}')
return json_loads(json_dumps(original))
"#,
    )
    .await?;
    assert_eq!(response.result["emoji"], "\u{2764}");

    let _ = client.cancel().await;
    Ok(())
}

async fn execute(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    code: &str,
) -> anyhow::Result<ExecuteResponse> {
    let result = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "gambi_execute_escalated".to_string().into(),
            arguments: Some(
                serde_json::json!({ "code": code })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            task: None,
        })
        .await?;

    if result.is_error.unwrap_or(false) {
        let first_text = result
            .content
            .first()
            .and_then(|content| content.as_text())
            .map(|text| text.text.clone())
            .unwrap_or_else(|| "gambi_execute returned an error".to_string());
        anyhow::bail!(first_text);
    }

    if let Some(structured) = result.structured_content {
        return Ok(serde_json::from_value(structured)?);
    }

    let first_text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .map(|text| text.text.clone())
        .ok_or_else(|| anyhow::anyhow!("gambi_execute did not return parseable output"))?;

    Ok(serde_json::from_str(&first_text)?)
}

async fn spawn_gambi_with_fixture(
    extra_env: BTreeMap<String, String>,
) -> anyhow::Result<(
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tempfile::TempDir,
)> {
    let bin = env!("CARGO_BIN_EXE_gambi");
    let temp = tempfile::tempdir()?;

    let gambi_config_dir = temp.path().join("gambi");
    std::fs::create_dir_all(&gambi_config_dir)?;

    let fixture_url = format!("stdio:///{bin}?arg=__fixture_progress_server", bin = bin);
    write_config(
        &gambi_config_dir.join("config.json"),
        &format!(
            "{{\n  \"servers\": [\n    {{ \"name\": \"fixture\", \"url\": \"{}\" }}\n  ]\n}}\n",
            fixture_url
        ),
    )?;
    write_config(&gambi_config_dir.join("tokens.json"), "{}\n")?;

    let transport = TokioChildProcess::new(Command::new(bin).configure(|cmd| {
        cmd.arg("serve");
        cmd.arg("--admin-port");
        cmd.arg("0");
        cmd.env("GAMBI_CONFIG_DIR", &gambi_config_dir);
        for (key, value) in &extra_env {
            cmd.env(key, value);
        }
    }))?;

    let client = ().serve(transport).await?;
    Ok((client, temp))
}

fn write_config(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
