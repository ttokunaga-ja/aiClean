use super::common::{messages, plain, root_path, skip};
use crate::model::{Inventory, Provider, Session};
use anyhow::Result;
use std::{fs, path::Path};

pub(super) fn inventory(input: &Path) -> Result<Inventory> {
    let mut output = Inventory {
        provider: Provider::Claude,
        sessions: vec![],
        warnings: vec![],
    };
    if !input.exists() {
        return Ok(output);
    }
    let root = root_path(input)?;
    let projects = root.join("projects");
    if !projects.exists() {
        return Ok(output);
    }
    plain(&root, &projects)?;
    let global_index = fs::read_dir(&projects)?.any(|e| {
        e.ok().is_some_and(|e| {
            e.file_type().is_ok_and(|t| !t.is_dir())
                && e.file_name()
                    .to_str()
                    .is_some_and(|name| name.contains("index") || name.contains("registration"))
        })
    });
    if global_index {
        output
            .warnings
            .push("unsupported global project index/registration; sessions retained".into());
    }
    for project in fs::read_dir(&projects)? {
        let project = project?.path();
        if !project.is_dir() {
            continue;
        }
        if let Err(e) = plain(&root, &project) {
            output.warnings.push(format!("{}: {e}", project.display()));
            continue;
        }
        let entries = fs::read_dir(&project)?
            .map(|x| x.map(|x| x.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        let indexed = entries.iter().any(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.contains("index") || s.contains("registration"))
        });
        if indexed {
            output.warnings.push(format!(
                "{}: unsupported project index/registration; sessions retained",
                project.display()
            ));
        }
        for file in &entries {
            if file.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = file.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let mut s = Session {
                provider: Provider::Claude,
                root: root.clone(),
                id: format!("{}/{}", project.file_name().unwrap().to_string_lossy(), id),
                last_message_ms: None,
                files: vec![file.clone()],
                databases: vec![],
                skip_reason: None,
            };
            if indexed || global_index {
                skip(&mut s, "unsupported project index/registration");
            }
            let child = project.join(id);
            if fs::symlink_metadata(&child).is_ok() {
                if let Err(e) = plain(&root, &child) {
                    skip(&mut s, e.to_string());
                } else {
                    for entry in walkdir::WalkDir::new(&child).follow_links(false) {
                        match entry {
                            Err(_) => skip(&mut s, "unreadable related session files"),
                            Ok(e) => {
                                if let Err(e) = plain(&root, e.path()) {
                                    skip(&mut s, e.to_string());
                                    continue;
                                }
                                if e.file_type().is_file() {
                                    s.files.push(e.path().to_path_buf());
                                }
                            }
                        }
                    }
                }
            }
            // Sidecars belonging to this precise session name travel with it.
            for side in &entries {
                if side != file
                    && side
                        .file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(|name| name.starts_with(&format!("{id}.")))
                {
                    if side.is_dir() {
                        skip(&mut s, "unsupported related sidecar directory");
                    } else {
                        s.files.push(side.clone());
                    }
                }
            }
            for source in s.files.clone() {
                if let Err(e) = plain(&root, &source) {
                    skip(&mut s, e.to_string());
                    continue;
                }
                if source.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    match messages(&source, true) {
                        Ok(Some(ts)) => {
                            s.last_message_ms =
                                Some(s.last_message_ms.map_or(ts, |old| old.max(ts)))
                        }
                        Ok(None) => skip(&mut s, "unknown related conversation timestamp"),
                        Err(e) => skip(&mut s, e.to_string()),
                    }
                }
            }
            if s.last_message_ms.is_none() {
                skip(&mut s, "unknown last conversation timestamp");
            }
            s.files.sort();
            s.files.dedup();
            output.sessions.push(s);
        }
    }
    output.sessions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(output)
}

/// Re-enumerate only this project's precise session group, never the full provider.
pub(super) fn recheck(original: &Session) -> Result<Session> {
    let root = root_path(&original.root)?;
    let (project_name, id) = original
        .id
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("unknown Claude session identity"))?;
    anyhow::ensure!(
        !id.contains('/') && !id.contains('\\') && !project_name.contains('\\'),
        "unsafe Claude identity"
    );
    let projects = root.join("projects");
    plain(&root, &projects)?;
    for entry in fs::read_dir(&projects)? {
        let entry = entry?;
        anyhow::ensure!(
            entry.file_type()?.is_dir()
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.contains("index") || name.contains("registration")),
            "global project index/registration changed or unsupported"
        );
    }
    let project = projects.join(project_name);
    plain(&root, &project)?;
    let entries = fs::read_dir(&project)?
        .map(|x| x.map(|x| x.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    anyhow::ensure!(
        !entries.iter().any(|p| p
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.contains("index") || s.contains("registration"))),
        "project index/registration changed or unsupported"
    );
    let main = project.join(format!("{id}.jsonl"));
    plain(&root, &main)?;
    let mut fresh = original.clone();
    fresh.files = vec![main];
    fresh.last_message_ms = None;
    fresh.skip_reason = None;
    let child = project.join(id);
    if fs::symlink_metadata(&child).is_ok() {
        plain(&root, &child)?;
        for entry in walkdir::WalkDir::new(child).follow_links(false) {
            let entry = entry?;
            plain(&root, entry.path())?;
            if entry.file_type().is_file() {
                fresh.files.push(entry.path().to_path_buf());
            }
        }
    }
    for side in entries {
        if side
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with(&format!("{id}.")))
            && !fresh.files.contains(&side)
        {
            plain(&root, &side)?;
            anyhow::ensure!(side.is_file(), "unsupported related sidecar directory");
            fresh.files.push(side);
        }
    }
    for path in &fresh.files {
        plain(&root, path)?;
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            let date = messages(path, true)?
                .ok_or_else(|| anyhow::anyhow!("unknown related conversation timestamp"))?;
            fresh.last_message_ms = Some(fresh.last_message_ms.map_or(date, |old| old.max(date)));
        }
    }
    anyhow::ensure!(
        fresh.last_message_ms.is_some(),
        "unknown conversation timestamp"
    );
    Ok(fresh)
}
