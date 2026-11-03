//! r0wmctl：最小 IPC 客户端（`docs/adr/0007`）。
//!
//! 用法：
//!   r0wmctl query focused
//!   r0wmctl query tags
//!   r0wmctl cmd "view" "3"
//!   r0wmctl subscribe

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use r0wm::ipc::{Query, Request, Topic};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let request = match parse(&args) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("{message}");
            return std::process::ExitCode::from(2);
        }
    };

    match send(request) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("r0wmctl: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn parse(args: &[String]) -> Result<Request, String> {
    match args {
        [verb, what] if verb == "query" => {
            let what = match what.as_str() {
                "clients" => Query::Clients,
                "tags" => Query::Tags,
                "outputs" => Query::Outputs,
                "focused" => Query::Focused,
                other => return Err(format!("未知查询：{other}")),
            };
            Ok(Request::Query { what })
        }
        [verb, action] if verb == "cmd" => Ok(Request::Command {
            action: action.clone(),
            arg: None,
        }),
        [verb, action, arg] if verb == "cmd" => Ok(Request::Command {
            action: action.clone(),
            arg: Some(arg.clone()),
        }),
        [verb, topics @ ..] if verb == "subscribe" => {
            let mut parsed = Vec::with_capacity(topics.len());
            for topic in topics {
                parsed.push(topic_from(topic).ok_or_else(|| format!("未知主题：{topic}"))?);
            }
            Ok(Request::Subscribe { topics: parsed })
        }
        _ => Err(usage()),
    }
}

fn topic_from(name: &str) -> Option<Topic> {
    Some(match name {
        "focus" => Topic::Focus,
        "tag" => Topic::Tag,
        "title" => Topic::Title,
        "urgent" => Topic::Urgent,
        "layout" => Topic::Layout,
        "output" => Topic::Output,
        _ => return None,
    })
}

fn usage() -> String {
    "用法：\n  r0wmctl query <clients|tags|outputs|focused>\n  \
     r0wmctl cmd <动作> [参数]\n  \
     r0wmctl subscribe [focus|tag|title|layout|output ...]"
        .to_string()
}

fn send(request: Request) -> anyhow::Result<()> {
    let path = r0wm::ipc::socket_path()
        .ok_or_else(|| anyhow::anyhow!("缺少 XDG_RUNTIME_DIR 或 WAYLAND_DISPLAY"))?;

    let mut stream = UnixStream::connect(&path)
        .map_err(|err| anyhow::anyhow!("连接 {} 失败：{err}", path.display()))?;

    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        println!("{}", line?);
    }
    Ok(())
}
