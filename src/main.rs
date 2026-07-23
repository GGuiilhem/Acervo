#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ignore::{WalkBuilder, WalkState};
use memchr::{memchr, memchr2, memmem};
use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Instant, UNIX_EPOCH},
};
use tauri::{ipc::Channel, State};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const MAX_RESULTS: usize = 100_000;
const MAX_CONTENT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Default)]
struct AppState {
    active_search: Arc<AtomicU64>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchRequest {
    directory: String,
    query: String,
    mode: String,
    use_regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    include_hidden: bool,
    include_subfolders: bool,
    file_pattern: String,
    min_size: Option<u64>,
    max_size: Option<u64>,
    min_modified: Option<u64>,
    max_modified: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaceRequest {
    paths: Vec<String>,
    query: String,
    replacement: String,
    use_regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    create_backup: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    name: String,
    path: String,
    size: u64,
    modified: u64,
    matches: usize,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum SearchEvent {
    Result {
        item: SearchResult,
    },
    Progress {
        scanned: usize,
        found: usize,
    },
    Finished {
        scanned: usize,
        found: usize,
        cancelled: bool,
    },
}

fn regex_matcher(req: &SearchRequest) -> Result<Regex, String> {
    let mut source = if req.use_regex {
        req.query.clone()
    } else {
        regex::escape(&req.query)
    };
    if req.whole_word {
        source = format!(r"\b(?:{})\b", source);
    }
    RegexBuilder::new(&source)
        .case_insensitive(!req.case_sensitive)
        .unicode(true)
        .build()
        .map_err(|e| format!("Expressão inválida: {e}"))
}

#[derive(Clone)]
enum SearchMatcher {
    Regex(Regex),
    Literal {
        needle: Vec<u8>,
        case_sensitive: bool,
    },
}

impl SearchMatcher {
    fn count(&self, haystack: &[u8]) -> usize {
        match self {
            Self::Regex(expression) => expression.find_iter(haystack).count(),
            Self::Literal {
                needle,
                case_sensitive: true,
            } => memmem::find_iter(haystack, needle).count(),
            Self::Literal {
                needle,
                case_sensitive: false,
            } => count_ascii_case_insensitive(haystack, needle),
        }
    }

    fn is_match(&self, haystack: &[u8]) -> bool {
        match self {
            Self::Regex(expression) => expression.is_match(haystack),
            Self::Literal {
                needle,
                case_sensitive: true,
            } => memmem::find(haystack, needle).is_some(),
            Self::Literal {
                needle,
                case_sensitive: false,
            } => count_ascii_case_insensitive(haystack, needle) > 0,
        }
    }
}

fn count_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let lower = needle[0].to_ascii_lowercase();
    let upper = needle[0].to_ascii_uppercase();
    let mut offset = 0;
    let mut count = 0;
    while offset + needle.len() <= haystack.len() {
        let next = if lower == upper {
            memchr(lower, &haystack[offset..])
        } else {
            memchr2(lower, upper, &haystack[offset..])
        };
        let Some(relative) = next else { break };
        let position = offset + relative;
        if position + needle.len() <= haystack.len()
            && haystack[position..position + needle.len()].eq_ignore_ascii_case(needle)
        {
            count += 1;
            offset = position + needle.len();
        } else {
            offset = position + 1;
        }
    }
    count
}

fn search_matcher(req: &SearchRequest) -> Result<SearchMatcher, String> {
    if !req.use_regex && !req.whole_word && (req.case_sensitive || req.query.is_ascii()) {
        Ok(SearchMatcher::Literal {
            needle: req.query.as_bytes().to_vec(),
            case_sensitive: req.case_sensitive,
        })
    } else {
        regex_matcher(req).map(SearchMatcher::Regex)
    }
}

fn wildcard_regex(patterns: &str) -> Result<Option<Regex>, String> {
    let values: Vec<_> = patterns
        .split([';', ','])
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "*")
        .collect();
    if values.is_empty() {
        return Ok(None);
    }
    let parts: Vec<String> = values
        .iter()
        .map(|value| {
            let escaped = regex::escape(value)
                .replace(r"\*", ".*")
                .replace(r"\?", ".");
            format!("(?:{escaped})")
        })
        .collect();
    RegexBuilder::new(&format!("^(?:{})$", parts.join("|")))
        .case_insensitive(true)
        .build()
        .map(Some)
        .map_err(|e| e.to_string())
}

fn validate(req: &SearchRequest) -> Result<(SearchMatcher, Option<Regex>), String> {
    if !Path::new(&req.directory).is_dir() {
        return Err("Selecione um diretório válido.".into());
    }
    if req.query.is_empty() {
        return Err("Digite o texto que deseja pesquisar.".into());
    }
    Ok((search_matcher(req)?, wildcard_regex(&req.file_pattern)?))
}

fn run_streaming_search(
    req: SearchRequest,
    expression: SearchMatcher,
    file_filter: Option<Regex>,
    id: u64,
    active: Arc<AtomicU64>,
    events: Channel<SearchEvent>,
) {
    let scanned = Arc::new(AtomicUsize::new(0));
    let found = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let last_progress_ms = Arc::new(AtomicU64::new(0));
    let mut builder = WalkBuilder::new(&req.directory);
    builder
        .hidden(!req.include_hidden)
        .max_depth(if req.include_subfolders {
            None
        } else {
            Some(1)
        })
        .follow_links(false)
        .threads(std::thread::available_parallelism().map_or(8, |v| v.get().clamp(4, 16)));

    let mode = Arc::new(req.mode);
    let min_size = req.min_size;
    let max_size = req.max_size;
    let min_modified = req.min_modified;
    let max_modified = req.max_modified;
    let expression = Arc::new(expression);
    let file_filter = Arc::new(file_filter);
    let walker = builder.build_parallel();
    walker.run(|| {
        let active = active.clone();
        let scanned = scanned.clone();
        let found = found.clone();
        let last_progress_ms = last_progress_ms.clone();
        let mode = mode.clone();
        let expression = expression.clone();
        let file_filter = file_filter.clone();
        let events = events.clone();
        Box::new(move |entry| {
            if active.load(Ordering::Relaxed) != id || found.load(Ordering::Relaxed) >= MAX_RESULTS
            {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let current = scanned.fetch_add(1, Ordering::Relaxed) + 1;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let previous_ms = last_progress_ms.load(Ordering::Relaxed);
            if elapsed_ms.saturating_sub(previous_ms) >= 100
                && last_progress_ms
                    .compare_exchange(
                        previous_ms,
                        elapsed_ms,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                let _ = events.send(SearchEvent::Progress {
                    scanned: current,
                    found: found.load(Ordering::Relaxed),
                });
            }
            let name = entry.file_name().to_string_lossy();
            if file_filter
                .as_ref()
                .as_ref()
                .is_some_and(|f| !f.is_match(name.as_bytes()))
            {
                return WalkState::Continue;
            }
            let Ok(metadata) = entry.metadata() else {
                return WalkState::Continue;
            };
            if min_size.is_some_and(|value| metadata.len() < value)
                || max_size.is_some_and(|value| metadata.len() > value)
            {
                return WalkState::Continue;
            }
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |value| value.as_secs());
            if min_modified.is_some_and(|value| modified < value)
                || max_modified.is_some_and(|value| modified > value)
            {
                return WalkState::Continue;
            }
            let matches = if mode.as_str() == "name" {
                usize::from(expression.is_match(name.as_bytes()))
            } else if metadata.len() <= MAX_CONTENT_BYTES {
                fs::read(entry.path()).map_or(0, |bytes| expression.count(&bytes))
            } else {
                0
            };
            if matches > 0 {
                found.fetch_add(1, Ordering::Relaxed);
                let _ = events.send(SearchEvent::Result {
                    item: SearchResult {
                        name: name.into_owned(),
                        path: entry.path().to_string_lossy().into_owned(),
                        size: metadata.len(),
                        modified,
                        matches,
                    },
                });
            }
            WalkState::Continue
        })
    });
    let cancelled = active.load(Ordering::Relaxed) != id;
    let _ = events.send(SearchEvent::Finished {
        scanned: scanned.load(Ordering::Relaxed),
        found: found.load(Ordering::Relaxed),
        cancelled,
    });
}

#[tauri::command]
fn start_search(
    req: SearchRequest,
    on_event: Channel<SearchEvent>,
    state: State<AppState>,
) -> Result<u64, String> {
    let (expression, file_filter) = validate(&req)?;
    let id = state.active_search.fetch_add(1, Ordering::SeqCst) + 1;
    let active = state.active_search.clone();
    std::thread::spawn(move || {
        run_streaming_search(req, expression, file_filter, id, active, on_event)
    });
    Ok(id)
}

#[tauri::command]
fn cancel_search(state: State<AppState>) {
    state.active_search.fetch_add(1, Ordering::SeqCst);
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    open::that_detached(path).map_err(|e| format!("Não foi possível abrir: {e}"))
}

#[tauri::command]
fn open_containing_folders(paths: Vec<String>) -> Result<usize, String> {
    let folders: std::collections::HashSet<PathBuf> = paths
        .iter()
        .filter_map(|value| Path::new(value).parent().map(Path::to_path_buf))
        .collect();
    if folders.is_empty() {
        return Err("Não foi possível identificar as pastas dos arquivos.".into());
    }
    for folder in &folders {
        std::process::Command::new("explorer.exe")
            .arg(folder)
            .spawn()
            .map_err(|e| format!("Não foi possível abrir {}: {e}", folder.display()))?;
    }
    Ok(folders.len())
}

#[tauri::command]
fn available_target(destination: &Path, source: &Path) -> Result<PathBuf, String> {
    let name = source.file_name().ok_or("Nome de arquivo inválido")?;
    let direct = destination.join(name);
    if !direct.exists() {
        return Ok(direct);
    }
    let stem = source
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("arquivo");
    let ext = source
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| format!(".{v}"))
        .unwrap_or_default();
    for index in 2..10_000 {
        let candidate = destination.join(format!("{stem} ({index}){ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("Não foi possível escolher um nome disponível no destino.".into())
}

fn backup_path(source: &Path) -> PathBuf {
    let direct = PathBuf::from(format!("{}.bak", source.display()));
    if !direct.exists() {
        return direct;
    }
    for index in 2..10_000 {
        let candidate = PathBuf::from(format!("{}.bak.{index}", source.display()));
        if !candidate.exists() {
            return candidate;
        }
    }
    direct
}

#[tauri::command]
fn replace_in_files(req: ReplaceRequest) -> Result<usize, String> {
    if req.paths.is_empty() {
        return Err("Selecione ao menos um arquivo.".into());
    }
    let search = SearchRequest {
        directory: ".".into(),
        query: req.query,
        mode: "content".into(),
        use_regex: req.use_regex,
        case_sensitive: req.case_sensitive,
        whole_word: req.whole_word,
        include_hidden: true,
        include_subfolders: true,
        file_pattern: "*".into(),
        min_size: None,
        max_size: None,
        min_modified: None,
        max_modified: None,
    };
    let expression = regex_matcher(&search)?;
    let mut changed = 0;
    for value in req.paths {
        let path = Path::new(&value);
        let original =
            fs::read(path).map_err(|e| format!("Erro ao ler {}: {e}", path.display()))?;
        if !expression.is_match(&original) {
            continue;
        }
        let replaced = if req.use_regex {
            expression.replace_all(&original, req.replacement.as_bytes())
        } else {
            expression.replace_all(
                &original,
                regex::bytes::NoExpand(req.replacement.as_bytes()),
            )
        };
        if req.create_backup {
            fs::copy(path, backup_path(path))
                .map_err(|e| format!("Erro ao criar backup de {}: {e}", path.display()))?;
        }
        fs::write(path, replaced.as_ref())
            .map_err(|e| format!("Erro ao salvar {}: {e}", path.display()))?;
        changed += 1;
    }
    Ok(changed)
}

#[tauri::command]
fn transfer_files(
    paths: Vec<String>,
    destination: String,
    move_files: bool,
) -> Result<usize, String> {
    let destination = Path::new(&destination);
    if !destination.is_dir() {
        return Err("Selecione uma pasta de destino válida.".into());
    }
    let mut completed = 0;
    for value in paths {
        let source = Path::new(&value);
        if !source.is_file() {
            continue;
        }
        let target = available_target(destination, source)?;
        if move_files {
            if fs::rename(source, &target).is_err() {
                fs::copy(source, &target)
                    .map_err(|e| format!("Erro ao mover {}: {e}", source.display()))?;
                fs::remove_file(source)
                    .map_err(|e| format!("Erro ao concluir a movimentação: {e}"))?;
            }
        } else {
            fs::copy(source, &target)
                .map_err(|e| format!("Erro ao copiar {}: {e}", source.display()))?;
        }
        completed += 1;
    }
    Ok(completed)
}

#[tauri::command]
fn zip_files(paths: Vec<String>, destination: String) -> Result<usize, String> {
    if paths.is_empty() {
        return Err("Selecione ao menos um arquivo.".into());
    }
    let destination = if destination.to_lowercase().ends_with(".zip") {
        PathBuf::from(destination)
    } else {
        PathBuf::from(format!("{destination}.zip"))
    };
    let output = fs::File::create(&destination)
        .map_err(|e| format!("Não foi possível criar {}: {e}", destination.display()))?;
    let mut archive = ZipWriter::new(output);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut used_names = std::collections::HashSet::new();
    let mut completed = 0;
    for value in paths {
        let source = Path::new(&value);
        if !source.is_file() {
            continue;
        }
        let original = source
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("arquivo");
        let mut name = original.to_string();
        let stem = source
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("arquivo");
        let ext = source
            .extension()
            .and_then(|v| v.to_str())
            .map(|v| format!(".{v}"))
            .unwrap_or_default();
        let mut index = 2;
        while !used_names.insert(name.clone()) {
            name = format!("{stem} ({index}){ext}");
            index += 1;
        }
        archive
            .start_file(&name, options)
            .map_err(|e| format!("Erro ao incluir {name}: {e}"))?;
        let mut input =
            fs::File::open(source).map_err(|e| format!("Erro ao ler {}: {e}", source.display()))?;
        std::io::copy(&mut input, &mut archive)
            .map_err(|e| format!("Erro ao compactar {name}: {e}"))?;
        completed += 1;
    }
    archive
        .finish()
        .map_err(|e| format!("Erro ao finalizar o ZIP: {e}"))?;
    Ok(completed)
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            start_search,
            cancel_search,
            open_path,
            open_containing_folders,
            transfer_files,
            zip_files,
            replace_in_files
        ])
        .run(tauri::generate_context!())
        .expect("erro ao executar Acervo");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_ID: AtomicUsize = AtomicUsize::new(0);
    #[test]
    fn supports_wildcards() {
        let filter = wildcard_regex("*.txt; *.md").unwrap().unwrap();
        assert!(filter.is_match(b"relatorio.TXT"));
        assert!(filter.is_match(b"notas.md"));
        assert!(!filter.is_match(b"foto.png"));
    }
    #[test]
    fn creates_case_insensitive_literal_matcher() {
        let req = SearchRequest {
            directory: ".".into(),
            query: "Pesquisa".into(),
            mode: "content".into(),
            use_regex: false,
            case_sensitive: false,
            whole_word: false,
            include_hidden: false,
            include_subfolders: true,
            file_pattern: "*".into(),
            min_size: None,
            max_size: None,
            min_modified: None,
            max_modified: None,
        };
        assert!(regex_matcher(&req)
            .unwrap()
            .is_match("uma PESQUISA rápida".as_bytes()));
    }

    #[test]
    fn optimized_literal_counts_without_regex() {
        let req = SearchRequest {
            directory: ".".into(),
            query: "<matricula>123</matricula>".into(),
            mode: "content".into(),
            use_regex: false,
            case_sensitive: false,
            whole_word: false,
            include_hidden: false,
            include_subfolders: true,
            file_pattern: "*.xml".into(),
            min_size: None,
            max_size: None,
            min_modified: None,
            max_modified: None,
        };
        let matcher = search_matcher(&req).unwrap();
        assert_eq!(
            matcher.count(b"<MATRICULA>123</MATRICULA><matricula>123</matricula>"),
            2
        );
        assert!(matches!(matcher, SearchMatcher::Literal { .. }));
    }

    #[test]
    fn replaces_selected_file_and_creates_backup() {
        let path = std::env::temp_dir().join(format!(
            "acervo-replace-{}-{}.txt",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, "texto antigo e outro texto antigo").unwrap();
        let changed = replace_in_files(ReplaceRequest {
            paths: vec![path.to_string_lossy().into_owned()],
            query: "texto antigo".into(),
            replacement: "texto novo".into(),
            use_regex: false,
            case_sensitive: false,
            whole_word: false,
            create_backup: true,
        })
        .unwrap();
        assert_eq!(changed, 1);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "texto novo e outro texto novo"
        );
        let created_backup = PathBuf::from(format!("{}.bak", path.display()));
        assert!(created_backup.exists());
        fs::remove_file(path).unwrap();
        fs::remove_file(created_backup).unwrap();
    }

    #[test]
    fn creates_zip_with_selected_files() {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("acervo-zip-{}-{id}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let first = root.join("primeiro.txt");
        let second = root.join("segundo.txt");
        let output = root.join("selecionados.zip");
        fs::write(&first, "um").unwrap();
        fs::write(&second, "dois").unwrap();
        let count = zip_files(
            vec![
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            output.to_string_lossy().into_owned(),
        )
        .unwrap();
        assert_eq!(count, 2);
        let mut archive = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);
        assert!(archive.by_name("primeiro.txt").is_ok());
        drop(archive);
        fs::remove_dir_all(root).unwrap();
    }
}
