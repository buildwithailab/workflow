use crate::core::{id, string, Core};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub fn folder_id(project: &Value, value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let exists = value.as_str().is_some_and(|id| {
        project["sessionFolders"]
            .as_array()
            .is_some_and(|folders| folders.iter().any(|folder| folder["id"] == id))
    });
    if !exists {
        bail!("Oturum klasörü bu projede bulunamadı.");
    }
    Ok(value.clone())
}

pub fn dispatch(core: &Core, action: &str, args: &Value) -> Result<Value> {
    let _operation = core.operations.lock().unwrap();
    let mut db = core.db.lock().unwrap();
    let mut updated = db.clone();
    let project_id = string(args, "projectId")?;
    let project = updated["projects"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|project| project["id"] == project_id && project["removed"] != true)
        .context("Proje yok")?;

    let result = match action {
        "folder_create" | "folder_rename" => {
            let name = string(args, "name")?.trim();
            if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
                bail!("Klasör adı 1–80 karakter olmalı.");
            }
            let rename = action == "folder_rename";
            let key = if rename {
                string(args, "folderId")?.to_owned()
            } else {
                id()
            };
            if project["sessionFolders"].is_null() {
                project["sessionFolders"] = json!([]);
            }
            let folders = project["sessionFolders"].as_array_mut().unwrap();
            let normalized = name.to_lowercase();
            if folders.iter().any(|folder| {
                folder["id"] != key
                    && folder["name"].as_str().is_some_and(|name| name.to_lowercase() == normalized)
            }) {
                bail!("Bu adda bir oturum klasörü zaten var.");
            }
            let folder = json!({"id":key,"name":name});
            if rename {
                let stored = folders.iter_mut().find(|item| item["id"] == key)
                    .context("Oturum klasörü bu projede bulunamadı.")?;
                *stored = folder.clone();
            } else {
                folders.push(folder.clone());
            }
            folder
        }
        "folder_remove" => {
            let key = string(args, "folderId")?;
            folder_id(project, &json!(key))?;
            project["sessionFolders"].as_array_mut().unwrap().retain(|item| item["id"] != key);
            for session in updated["sessions"].as_array_mut().unwrap() {
                if session["projectId"] == project_id && session["folderId"] == key {
                    session["folderId"] = Value::Null;
                }
            }
            json!(true)
        }
        "folder_move_session" => {
            let destination = folder_id(project, args.get("folderId").context("Oturum klasörü seç.")?)?;
            let session_id = string(args, "id")?;
            let session = updated["sessions"].as_array_mut().unwrap().iter_mut()
                .find(|session| session["projectId"] == project_id && session["id"] == session_id)
                .context("Oturum bulunamadı")?;
            session["folderId"] = destination;
            session.clone()
        }
        _ => bail!("Oturum klasörü işlemi geçersiz"),
    };
    core.save_workspace(&updated)?;
    *db = updated;
    drop(db);
    core.emit("workspace", core.snapshot());
    Ok(result)
}
