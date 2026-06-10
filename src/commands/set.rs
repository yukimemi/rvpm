use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_set(
    query: Option<String>,
    lazy: Option<bool>,
    merge: Option<bool>,
    on_cmd: Option<String>,
    on_ft: Option<String>,
    on_map: Option<String>,
    on_event: Option<String>,
    on_path: Option<String>,
    on_source: Option<String>,
    rev: Option<String>,
) -> Result<bool> {
    let config_path = rvpm_config_path();
    let toml_content = std::fs::read_to_string(&config_path)?;
    let config = parse_config(&toml_content)?;

    let Some(selected_repo_url) =
        select_plugin_url(&config.plugins, query.as_deref(), "Select plugin to set")?
    else {
        return Ok(false);
    };

    println!("\n>> Setting options for: {}", selected_repo_url);
    let mut doc = toml_content.parse::<DocumentMut>()?;
    let mut modified = false;

    let any_flag_set = lazy.is_some()
        || merge.is_some()
        || on_cmd.is_some()
        || on_ft.is_some()
        || on_map.is_some()
        || on_event.is_some()
        || on_path.is_some()
        || on_source.is_some()
        || rev.is_some();

    if any_flag_set {
        // Option<String> → Result<Option<Vec<String>>> へ (malformed JSON はエラー)
        let maybe_parse = |raw: Option<String>| -> Result<Option<Vec<String>>> {
            raw.map(|s| parse_cli_string_list(&s)).transpose()
        };

        update_plugin_config(
            &mut doc,
            &selected_repo_url,
            lazy,
            merge,
            maybe_parse(on_cmd)?,
            maybe_parse(on_ft)?,
            rev,
        )?;
        // on_map は table 形式 (mode/desc) をサポートするため専用パーサを通す
        if let Some(raw) = on_map {
            let specs = parse_on_map_cli(&raw)?;
            set_plugin_map_field(&mut doc, &selected_repo_url, specs)?;
        }
        if let Some(items) = maybe_parse(on_event)? {
            set_plugin_list_field(&mut doc, &selected_repo_url, "on_event", items)?;
        }
        if let Some(items) = maybe_parse(on_path)? {
            set_plugin_list_field(&mut doc, &selected_repo_url, "on_path", items)?;
        }
        if let Some(items) = maybe_parse(on_source)? {
            set_plugin_list_field(&mut doc, &selected_repo_url, "on_source", items)?;
        }
        modified = true;
    } else {
        // 現在のプラグインを探して既存値をプレフィルに使う
        let current_plugin = config
            .plugins
            .iter()
            .find(|p| p.url == selected_repo_url)
            .cloned();
        let list_field_value = |field: &str| -> String {
            let Some(p) = current_plugin.as_ref() else {
                return String::new();
            };
            // on_map は MapSpec の lhs だけを列挙する (mode/desc は手書き編集に委ねる)
            let items: Option<Vec<String>> = match field {
                "on_cmd" => p.on_cmd.clone(),
                "on_ft" => p.on_ft.clone(),
                "on_map" => p
                    .on_map
                    .as_ref()
                    .map(|v| v.iter().map(|m| m.lhs.clone()).collect()),
                "on_event" => p.on_event.clone(),
                "on_path" => p.on_path.clone(),
                "on_source" => p.on_source.clone(),
                _ => None,
            };
            items.map(|v| v.join(", ")).unwrap_or_default()
        };

        const EDITOR_SENTINEL: &str = "[ Open config.toml in $EDITOR ]";
        let options = vec![
            EDITOR_SENTINEL,
            "lazy",
            "merge",
            "on_cmd",
            "on_ft",
            "on_map",
            "on_event",
            "on_path",
            "on_source",
            "rev",
        ];
        let selection = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Select option to set")
            .default(0)
            .items(&options)
            .interact_opt()?;
        match selection {
            Some(index) => {
                match options[index] {
                    s if s == EDITOR_SENTINEL => {
                        // 対応 editor なら plugin の url 行にジャンプ
                        let line = find_plugin_line_in_toml(&toml_content, &selected_repo_url);
                        let cz = read_chezmoi_flag(&config_path);
                        let ep = chezmoi::write_path(cz, &config_path).await;
                        open_editor_at_line(&ep, line)?;
                        chezmoi::apply(&ep, &config_path).await;
                        // ユーザーが何を編集したか分からないので常に変更ありと見なす
                        return Ok(true);
                    }
                    "lazy" | "merge" => {
                        let current = current_plugin
                            .as_ref()
                            .map(|p| {
                                if options[index] == "lazy" {
                                    p.lazy
                                } else {
                                    p.merge
                                }
                            })
                            .unwrap_or(false);
                        let default_idx = if current { 0 } else { 1 };
                        let val = Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                            .with_prompt(format!(
                                "Set {} to (current: {})",
                                options[index], current
                            ))
                            .items(["true", "false"])
                            .default(default_idx)
                            .interact_opt()?;
                        if let Some(v) = val {
                            update_plugin_config(
                                &mut doc,
                                &selected_repo_url,
                                if options[index] == "lazy" {
                                    Some(v == 0)
                                } else {
                                    None
                                },
                                if options[index] == "merge" {
                                    Some(v == 0)
                                } else {
                                    None
                                },
                                None,
                                None,
                                None,
                            )?;
                            modified = true;
                        } else {
                            return Ok(false);
                        }
                    }
                    "on_map" => {
                        // on_map は table 形式 (mode/desc) もあるので edit mode を先に聞く
                        let modes = &[
                            "Edit lhs list only (CLI, mode/desc lost)",
                            "Open config.toml in $EDITOR",
                        ];
                        let mode_sel =
                            Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                                .with_prompt("on_map edit mode")
                                .items(modes)
                                .default(0)
                                .interact_opt()?;
                        match mode_sel {
                            Some(0) => {
                                // CLI: lhs のみ編集 (既存の簡易フロー)
                                let existing = list_field_value("on_map");
                                let val = read_input_with_esc(
                                    "Enter on_map lhs values (comma separated, Esc to cancel)",
                                    &existing,
                                )?;
                                match val {
                                    Some(v) if !v.is_empty() => {
                                        let items: Vec<String> = v
                                            .split(',')
                                            .map(|s| s.trim().to_string())
                                            .filter(|s| !s.is_empty())
                                            .collect();
                                        set_plugin_list_field(
                                            &mut doc,
                                            &selected_repo_url,
                                            "on_map",
                                            items,
                                        )?;
                                        modified = true;
                                    }
                                    _ => return Ok(false),
                                }
                            }
                            Some(1) => {
                                let line =
                                    find_plugin_line_in_toml(&toml_content, &selected_repo_url);
                                let cz = read_chezmoi_flag(&config_path);
                                let ep = chezmoi::write_path(cz, &config_path).await;
                                open_editor_at_line(&ep, line)?;
                                chezmoi::apply(&ep, &config_path).await;
                                return Ok(true);
                            }
                            _ => return Ok(false),
                        }
                    }
                    field @ ("on_cmd" | "on_ft" | "on_event" | "on_path" | "on_source") => {
                        let existing = list_field_value(field);
                        let val = read_input_with_esc(
                            &format!("Enter {} (comma separated, Esc to cancel)", field),
                            &existing,
                        )?;
                        match val {
                            Some(v) if !v.is_empty() => {
                                let items: Vec<String> = v
                                    .split(',')
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty())
                                    .collect();
                                set_plugin_list_field(&mut doc, &selected_repo_url, field, items)?;
                                modified = true;
                            }
                            _ => return Ok(false),
                        }
                    }
                    "rev" => {
                        let existing = current_plugin
                            .as_ref()
                            .and_then(|p| p.rev.clone())
                            .unwrap_or_default();
                        let val = read_input_with_esc(
                            "Enter rev (branch/tag/hash, Esc to cancel)",
                            &existing,
                        )?;
                        match val {
                            Some(v) if !v.is_empty() => {
                                update_plugin_config(
                                    &mut doc,
                                    &selected_repo_url,
                                    None,
                                    None,
                                    None,
                                    None,
                                    Some(v),
                                )?;
                                modified = true;
                            }
                            _ => return Ok(false),
                        }
                    }
                    _ => {}
                }
            }
            None => return Ok(false),
        }
    }

    if modified {
        let chezmoi_enabled = read_chezmoi_flag(&config_path);
        chezmoi::write_routed(chezmoi_enabled, &config_path, doc.to_string()).await?;
        println!("Updated config for: {}", selected_repo_url);
        return Ok(true);
    }
    Ok(false)
}

/// ESC キーで None を返し、Enter キーで入力文字列を Some で返すテキスト入力。
/// crossterm の raw mode を一時的に有効化して使用する。
/// `initial` を渡すと、その値を初期入力として表示・編集できる。
fn read_input_with_esc(prompt: &str, initial: &str) -> Result<Option<String>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    use std::io::Write;

    let mut input = String::from(initial);
    print!("{}: {}", prompt, input);
    std::io::stdout().flush()?;

    crossterm::terminal::enable_raw_mode()?;

    let result = loop {
        match crossterm::event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => {
                    break Ok(None);
                }
                KeyCode::Enter => {
                    break Ok(Some(input.clone()));
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow::anyhow!("Interrupted"));
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    print!("{}", c);
                    std::io::stdout().flush()?;
                }
                KeyCode::Backspace if !input.is_empty() => {
                    input.pop();
                    print!("\x08 \x08");
                    std::io::stdout().flush()?;
                }
                _ => {}
            },
            _ => {}
        }
    };

    crossterm::terminal::disable_raw_mode()?;
    println!();
    result
}

/// config.toml 上で指定プラグイン (url 一致) の `url = "..."` 行の行番号 (1-indexed) を返す。
/// 見つからなければ 1 を返す (ファイル先頭)。
/// whitespace の入り方に寛容: `url="..."`, `url = "..."`, `url  =   "..."` など全部拾う。
fn find_plugin_line_in_toml(toml_content: &str, url: &str) -> usize {
    let needle = format!("\"{}\"", url);
    for (i, line) in toml_content.lines().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("url") {
            continue;
        }
        // "url" の後は空白 or "=" しか来ないはず (他のフィールド名は "url..." で始まらない)
        let rest = trimmed["url".len()..].trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        if line.contains(&needle) {
            return i + 1;
        }
    }
    1
}

fn update_plugin_config(
    doc: &mut DocumentMut,
    url: &str,
    lazy: Option<bool>,
    merge: Option<bool>,
    on_cmd: Option<Vec<String>>,
    on_ft: Option<Vec<String>>,
    rev: Option<String>,
) -> Result<()> {
    if let Some(l) = lazy {
        let plugins = doc["plugins"]
            .as_array_of_tables_mut()
            .context("plugins is not an array of tables")?;
        let plugin_table = plugins
            .iter_mut()
            .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
            .context("Could not find plugin in toml_edit document")?;
        plugin_table["lazy"] = value(l);
    }
    if let Some(m) = merge {
        let plugins = doc["plugins"]
            .as_array_of_tables_mut()
            .context("plugins is not an array of tables")?;
        let plugin_table = plugins
            .iter_mut()
            .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
            .context("Could not find plugin in toml_edit document")?;
        plugin_table["merge"] = value(m);
    }
    if let Some(cmds) = on_cmd {
        set_plugin_list_field(doc, url, "on_cmd", cmds)?;
    }
    if let Some(fts) = on_ft {
        set_plugin_list_field(doc, url, "on_ft", fts)?;
    }
    if let Some(r) = rev {
        let plugins = doc["plugins"]
            .as_array_of_tables_mut()
            .context("plugins is not an array of tables")?;
        let plugin_table = plugins
            .iter_mut()
            .find(|p| p.get("url").and_then(|v| v.as_str()) == Some(url))
            .context("Could not find plugin in toml_edit document")?;
        plugin_table["rev"] = value(r);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml_edit::DocumentMut;

    #[test]
    fn test_find_plugin_line_in_toml_basic() {
        let toml = "[options]\n\n[[plugins]]\nurl = \"owner/a\"\nlazy = true\n\n[[plugins]]\nurl = \"owner/b\"\n";
        //            1         2  3             4             5           6  7             8
        assert_eq!(find_plugin_line_in_toml(toml, "owner/a"), 4);
        assert_eq!(find_plugin_line_in_toml(toml, "owner/b"), 8);
    }

    #[test]
    fn test_find_plugin_line_in_toml_handles_whitespace_variants() {
        let toml = "[[plugins]]\nurl=\"owner/a\"\n\n[[plugins]]\nurl  =   \"owner/b\"\n";
        assert_eq!(find_plugin_line_in_toml(toml, "owner/a"), 2);
        assert_eq!(find_plugin_line_in_toml(toml, "owner/b"), 5);
    }

    #[test]
    fn test_find_plugin_line_in_toml_missing_falls_back_to_one() {
        let toml = "[[plugins]]\nurl = \"owner/a\"\n";
        assert_eq!(find_plugin_line_in_toml(toml, "owner/nonexistent"), 1);
    }

    #[test]
    fn test_find_plugin_line_in_toml_ignores_substring_matches() {
        // "owner/ab" should not be matched when searching for "owner/a"
        let toml = "[[plugins]]\nurl = \"owner/ab\"\n\n[[plugins]]\nurl = \"owner/a\"\n";
        assert_eq!(find_plugin_line_in_toml(toml, "owner/a"), 5);
    }

    #[test]
    fn test_update_plugin_config() {
        let toml = r#"[[plugins]]
url = "test/plugin"
lazy = false"#;
        let mut doc = toml.parse::<DocumentMut>().unwrap();
        update_plugin_config(
            &mut doc,
            "test/plugin",
            Some(true),
            Some(true),
            None,
            None,
            Some("v1.0".to_string()),
        )
        .unwrap();
        let result = doc.to_string();
        assert!(result.contains("lazy = true"));
        assert!(result.contains("merge = true"));
        assert!(result.contains("rev = \"v1.0\""));
    }
}
