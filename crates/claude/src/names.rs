//! Session names: with Settings → Management panel → "name
//! every session" on, every Claude session in a Giverny tab is
//! named at its start, before its first turn: its
//! `SessionStart` hook draws it a first name ([`draw_name`]:
//! sessions left to pick one kept picking the same few), gives it the
//! surname of where it was born ([`surname`]), records it, and
//! tells the session. The status line shows it beside the model from the
//! start: `Opus 5.5 (Rotem Giverny)`.
//!
//! **The surname is the session's category**, as surnames
//! once were: the rail category its tab sits in when it is named, as the person
//! wrote it, with the first letter capitalised (`giverny` → `Giverny`,
//! `MyApp` stays), else, in no category, its repo. Not drawn. It is fixed
//! at birth like a real one: recorded with the first name and never
//! recomputed, so moving or renaming the tab renames nobody, and a resumed
//! session keeps the name it had, an older drawn one (`Rotem Ashby`) too.
//!
//! A name is given once: `giverny-manage name` prints the one recorded. A
//! session may still give its own: a first name alone takes its category
//! surname, a full name passed in (a revived session keeping its old one) is
//! kept as given, and naming again with the same first name keeps the
//! surname already recorded, so nobody is renamed halfway through.
//!
//! **No two live sessions share a first name**: people tell their
//! sessions apart by it, two managers in one category above all. A session
//! is live while its claude runs, by Claude Code's own registry
//! ([`live_sessions`]), so a first name comes free again once its session
//! ends and the pool never fills up over months. A drawn first name is
//! drawn from the pool less the ones live sessions hold ([`draw_name`]); one
//! given by hand that another live session holds is refused, naming the
//! holder ([`record`]); and a resumed session takes its conversation's old
//! name back only while nobody live took that first name meanwhile
//! ([`find`]), else it is drawn a new one. The same session, or another id
//! of the same conversation, re-taking its own name is fine. A full name
//! need not be unique over time: a second Rotem Giverny once the first has
//! ended is fine.
//!
//! **Where a name lives.** Beside the feeds, never in one: a session that
//! only names itself is no manager, and a feed file would make it one.
//! `<feeds>/names/<session>.json` holds `{session, name, room, root}` (the
//! room its tab can be titled after, [`ROOMS`]), and
//! `<feeds>/names/roots/<root>.json` the same, keyed by the conversation's
//! root ([`crate::continuation`]): a resumed session has a new id but the same
//! root, so the status line finds its name there and copies it under the new
//! id. A session named at its start has no root yet (its
//! transcript holds no turn), so its first prompt and the status line stamp
//! one in as soon as the transcript has it ([`keep_root`]). `<feeds>/names/on`
//! exists while the switch is on; the app writes and removes it, and the
//! status line and the session-start hook read it.
//!
//! **When a session is named**. Claude Code reads a plugin's
//! `hooks.json` once, when its process starts (and on `/reload-plugins`),
//! never on `/clear`, a compaction or a resume, measured on 2.1.296: a claude
//! started before the plugin gained a hook never runs it. So the plugin's
//! `SessionStart` hook is always registered and reads the switch itself
//! ([`session_start`]), and flipping it reaches every session started since
//! at its next start. A session whose process predates the hook, or that
//! started with the switch off, is named on its next prompt instead, by the
//! `UserPromptSubmit` hook it does run ([`first_prompt`]). Either way a
//! session is named and told once: `<feeds>/names/asked/<session>` marks the
//! ones told.
//!
//! **A `/clear` keeps the name**. The session a `/clear` starts is a new
//! one, but it is the same agent to the person: it takes back the name and
//! room of the session it cleared in that tab ([`after_clear`]), found by
//! `<feeds>/names/tabs/<GIVERNY_TAB_ID>`, which holds the id of the last
//! session told its name there. Nothing else carries over: what is kept by
//! session id stays with the old one. A new agent means a new tab, or a new
//! claude in it.
//!
//! **`/resume` shows it**: a named session's custom title is
//! kept `<Name> · <the text the picker would show without it>`
//! ([`keep_title`]), one appended transcript line at a time; a title the
//! person gave is left alone.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The names directory under the feed directory.
pub const DIR: &str = "names";
/// The switch, as a file in [`DIR`]: present while sessions are named and
/// their names shown.
const ON: &str = "on";
/// Under [`DIR`], an empty file per session already told its name
/// ([`tell`]), so the prompt hook's fallback ([`first_prompt`]) never tells
/// it twice.
const ASKED: &str = "asked";
/// Under [`DIR`], a file per tab (named by its `GIVERNY_TAB_ID`) holding the
/// id of the last session told its name there: who a `/clear` in that tab
/// cleared ([`after_clear`]).
const TABS: &str = "tabs";
/// Under [`DIR`], the lock every rewrite of a names document is made under
/// ([`lock`]), so two sessions drawing at once
/// never share a first name and a room is drawn once.
const LOCK: &str = ".lock";
/// How long a names document just written counts its session as live
/// though Claude Code's registry does not list it yet: a
/// session drawing in the instant another starts sees its name.
const FRESH: std::time::Duration = std::time::Duration::from_secs(30);

/// Giverny's own first names, drawn for a session that asks for a name
/// without giving one: sessions left to pick kept picking the
/// same few. Personal names of many origins, short and easy to say; none a
/// famous single-name figure, a tool or language (`Ruby`, `Zig`), or a word
/// that would read as part of a log line (`May`, `Max`, `Dev`).
pub const FIRST_NAMES: &[&str] = &[
    "Aarav",
    "Abel",
    "Abena",
    "Adaeze",
    "Ade",
    "Adi",
    "Aditi",
    "Agata",
    "Agnes",
    "Aiko",
    "Akira",
    "Alba",
    "Alden",
    "Alexis",
    "Ama",
    "Amaia",
    "Amara",
    "Amir",
    "Amit",
    "Anh",
    "Anika",
    "Anjali",
    "Anke",
    "Ansel",
    "Anya",
    "Arda",
    "Arjun",
    "Arlo",
    "Arne",
    "Arthur",
    "Asha",
    "Astrid",
    "Audrey",
    "Ayla",
    "Ayo",
    "Bahar",
    "Bao",
    "Barnaby",
    "Bastian",
    "Beatrix",
    "Beau",
    "Benicio",
    "Bisi",
    "Bjorn",
    "Bogdan",
    "Borys",
    "Bram",
    "Briony",
    "Britt",
    "Bruno",
    "Calla",
    "Callum",
    "Camila",
    "Cassia",
    "Catalina",
    "Cecily",
    "Cem",
    "Cerys",
    "Chiamaka",
    "Chidi",
    "Ciaran",
    "Clara",
    "Cole",
    "Cormac",
    "Cosima",
    "Cyrus",
    "Dafna",
    "Dagny",
    "Daiki",
    "Dalia",
    "Damon",
    "Danica",
    "Daphne",
    "Darina",
    "Dario",
    "Dariush",
    "Darya",
    "Dayo",
    "Declan",
    "Defne",
    "Delphine",
    "Diego",
    "Dimitra",
    "Dirk",
    "Dmitri",
    "Dorothea",
    "Dragan",
    "Eamon",
    "Ebba",
    "Edda",
    "Edith",
    "Edwin",
    "Eitan",
    "Ekene",
    "Elena",
    "Eleni",
    "Eli",
    "Elif",
    "Elin",
    "Elio",
    "Eliza",
    "Elke",
    "Ellis",
    "Elsie",
    "Elspeth",
    "Emi",
    "Emilio",
    "Emre",
    "Emrys",
    "Enzo",
    "Esme",
    "Esteban",
    "Etta",
    "Ewan",
    "Ezra",
    "Fabio",
    "Farah",
    "Felix",
    "Femi",
    "Femke",
    "Fenna",
    "Fergus",
    "Finn",
    "Fiona",
    "Flavia",
    "Folake",
    "Freya",
    "Frida",
    "Funmi",
    "Gael",
    "Gali",
    "Gareth",
    "Gideon",
    "Greta",
    "Gunnar",
    "Gus",
    "Gwen",
    "Hana",
    "Hanne",
    "Harriet",
    "Haruto",
    "Hattie",
    "Hedda",
    "Henrik",
    "Hester",
    "Hila",
    "Hiro",
    "Hoa",
    "Hollis",
    "Hyun",
    "Ido",
    "Idris",
    "Ignacio",
    "Ilias",
    "Ilse",
    "Ilya",
    "Imani",
    "Imelda",
    "Imogen",
    "Ines",
    "Ingo",
    "Ingrid",
    "Ione",
    "Irina",
    "Iris",
    "Ishaan",
    "Ishani",
    "Isla",
    "Ivana",
    "Ivo",
    "Ivor",
    "Jabari",
    "Jana",
    "Jarek",
    "Jasper",
    "Jens",
    "Jia",
    "Jiwoo",
    "Joaquin",
    "Jonah",
    "Jonas",
    "Joost",
    "Jude",
    "Julieta",
    "Juniper",
    "Kagiso",
    "Kai",
    "Kaito",
    "Kaja",
    "Kasia",
    "Kasimir",
    "Katya",
    "Kavya",
    "Keiko",
    "Keir",
    "Kenji",
    "Kerem",
    "Kian",
    "Kiran",
    "Kjell",
    "Klara",
    "Kofi",
    "Kojo",
    "Kostas",
    "Kwame",
    "Kwesi",
    "Lachlan",
    "Lars",
    "Layla",
    "Leandro",
    "Leda",
    "Leela",
    "Leif",
    "Lena",
    "Leo",
    "Lerato",
    "Lev",
    "Liesel",
    "Lindiwe",
    "Linh",
    "Linnea",
    "Lionel",
    "Lior",
    "Liora",
    "Lorcan",
    "Lorena",
    "Lorna",
    "Lottie",
    "Luca",
    "Lucia",
    "Luka",
    "Mabel",
    "Maeve",
    "Magnus",
    "Mairi",
    "Makena",
    "Malik",
    "Marek",
    "Maren",
    "Marisol",
    "Marit",
    "Marlowe",
    "Mateo",
    "Matteo",
    "Mavis",
    "Maya",
    "Meera",
    "Mei",
    "Mila",
    "Milena",
    "Milo",
    "Mina",
    "Minjun",
    "Mio",
    "Mirabel",
    "Mirek",
    "Nadia",
    "Nalini",
    "Nao",
    "Naoko",
    "Nasrin",
    "Nathaniel",
    "Navid",
    "Ned",
    "Neel",
    "Nell",
    "Neta",
    "Nia",
    "Nico",
    "Nikhil",
    "Nikos",
    "Nils",
    "Nina",
    "Nkiru",
    "Noa",
    "Noam",
    "Nolan",
    "Nora",
    "Obi",
    "Odile",
    "Oksana",
    "Oliver",
    "Omar",
    "Omri",
    "Oona",
    "Oren",
    "Orla",
    "Oskar",
    "Otis",
    "Ottilie",
    "Otto",
    "Paloma",
    "Parisa",
    "Pavel",
    "Percy",
    "Petra",
    "Phoebe",
    "Pieter",
    "Pilar",
    "Pippa",
    "Priya",
    "Quan",
    "Quentin",
    "Quinn",
    "Radek",
    "Rahul",
    "Ralf",
    "Rami",
    "Ramona",
    "Ravi",
    "Renata",
    "Reuben",
    "Reza",
    "Rhys",
    "Riku",
    "Rin",
    "Rocio",
    "Rodrigo",
    "Rohan",
    "Ronan",
    "Roni",
    "Rosa",
    "Rosalind",
    "Rotem",
    "Rowan",
    "Rufus",
    "Runa",
    "Ruth",
    "Sadie",
    "Saki",
    "Sameer",
    "Samira",
    "Sanjay",
    "Sanne",
    "Sasha",
    "Selim",
    "Seren",
    "Seung",
    "Shira",
    "Shirin",
    "Sigrid",
    "Silas",
    "Simeon",
    "Sipho",
    "Solveig",
    "Soraya",
    "Stavros",
    "Stefan",
    "Stellan",
    "Sunil",
    "Sven",
    "Sybil",
    "Tabitha",
    "Takumi",
    "Tal",
    "Tamar",
    "Tamsin",
    "Tara",
    "Tarek",
    "Taro",
    "Tegan",
    "Tendai",
    "Teodor",
    "Tess",
    "Thalia",
    "Thandi",
    "Thao",
    "Thea",
    "Themba",
    "Theo",
    "Tiago",
    "Tobias",
    "Tobin",
    "Toby",
    "Tomasz",
    "Tove",
    "Tuan",
    "Tunde",
    "Ulla",
    "Una",
    "Ursula",
    "Valentin",
    "Valeria",
    "Varun",
    "Vera",
    "Vernon",
    "Vesna",
    "Vidar",
    "Vikram",
    "Walt",
    "Wanjiru",
    "Wei",
    "Wes",
    "Wilfred",
    "Willa",
    "Wim",
    "Winifred",
    "Wren",
    "Xanthe",
    "Ximena",
    "Xin",
    "Yael",
    "Yannick",
    "Yannis",
    "Yara",
    "Ylva",
    "Yuki",
    "Yuna",
    "Yuri",
    "Yusuf",
    "Yuto",
    "Zaira",
    "Zara",
    "Zawadi",
    "Zeke",
    "Zeynep",
    "Ziv",
    "Zoe",
    "Zofia",
    "Zola",
    "Zoran",
    "Zuri",
];

/// The rooms a session's tab can be titled after: with
/// Settings → tabs & titles → "name Claude tabs after their session" on, a
/// named session's tab reads as its place, `Rotem's Workshop`. One is drawn
/// per session at its birth and recorded beside its name ([`room`]), so the
/// title holds across refreshes and resumes. Rooms someone works in, one
/// word, short enough for the rail.
pub const ROOMS: &[&str] = &[
    "Atelier",
    "Attic",
    "Cabin",
    "Cellar",
    "Conservatory",
    "Den",
    "Forge",
    "Gallery",
    "Garden",
    "Greenhouse",
    "Lab",
    "Library",
    "Lighthouse",
    "Loft",
    "Nook",
    "Observatory",
    "Office",
    "Parlour",
    "Porch",
    "Shed",
    "Studio",
    "Study",
    "Tower",
    "Workshop",
];

/// A room from [`ROOMS`], at random.
pub fn draw_room() -> String {
    pick(ROOMS)
}

/// `name` at home in `room`: `Rotem Giverny` in the Workshop is
/// `Rotem's Workshop`. The first name alone, as a person calls their sessions.
pub fn place_of(name: &str, room: &str) -> Option<String> {
    let first = name.split_whitespace().next()?;
    let room = room.trim();
    (!room.is_empty()).then(|| format!("{first}'s {room}"))
}

/// The environment variable a Giverny tab's shell carries the name of the
/// rail category the tab sits in when its shell starts: what a
/// session born there takes as its surname ([`surname`]) when the app's
/// saved workspace cannot say where the tab sits now.
pub const CATEGORY_ENV: &str = "GIVERNY_CATEGORY";

/// The surname a session is born with: the name of the rail
/// category its tab sits in, as the person wrote it, first letter capitalised
/// (`giverny` → `Giverny`, `MyApp` stays); with no category, the repo it
/// runs in (`cwd`'s git root, a worktree counting as its main checkout,
/// else `cwd`'s own name). Not drawn: like a real surname it says where a
/// session comes from, and it is fixed at birth: recorded with the first
/// name, it is never recomputed, so moving or renaming the tab later
/// renames nobody. `feeds` is the app's feed directory, which finds its
/// `state/tabs.json`: the tab's category there comes first,
/// [`CATEGORY_ENV`] only when the tab is not found in it.
pub fn surname(feeds: &Path, cwd: Option<&Path>) -> String {
    surname_in(&|name| std::env::var(name).ok(), feeds, cwd)
}

/// [`surname`], reading the environment through `get`.
pub fn surname_in(
    get: &dyn Fn(&str) -> Option<String>,
    feeds: &Path,
    cwd: Option<&Path>,
) -> String {
    let var = |name: &str| crate::lineage::giverny_var_in(name, get);
    // The category the tab sits in now, by the workspace the app saves
    // within seconds of a change: a tab moved, or its
    // category renamed, since its shell started. The shell's own word for
    // it only where the app's state is out of reach (a WSL tab of a Windows
    // Giverny) or does not know the tab.
    tabs_file(feeds)
        .zip(var("GIVERNY_TAB_ID"))
        .and_then(|(tabs, tab)| tab_category(&tabs, &tab))
        .or_else(|| var(CATEGORY_ENV))
        .and_then(|c| as_surname(&c))
        .or_else(|| {
            let cwd = cwd
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok())?;
            as_surname(&crate::manage_history::repo_of("", &cwd)?)
        })
        .unwrap_or_else(|| "Giverny".to_string())
}

/// The app's saved workspace beside its feed directory
/// (`<base>/feeds` → `<base>/state/tabs.json`); none for a feed directory
/// that is not one of the app's own.
fn tabs_file(feeds: &Path) -> Option<PathBuf> {
    if feeds.file_name()? != "feeds" {
        return None;
    }
    Some(feeds.parent()?.join("state").join("tabs.json"))
}

/// The name of the category tab `tab` (`giverny-<n>`, as `GIVERNY_TAB_ID`
/// carries it) sits in, by the workspace saved at `tabs`. Read only: the
/// app owns the file.
fn tab_category(tabs: &Path, tab: &str) -> Option<String> {
    let n: u64 = tab.trim().strip_prefix("giverny-")?.parse().ok()?;
    let doc: Value = serde_json::from_slice(&std::fs::read(tabs).ok()?).ok()?;
    let ws = doc.get("workspace")?;
    let category = ws
        .get("tabs")?
        .as_array()?
        .iter()
        .find(|t| t.get("id").and_then(Value::as_u64) == Some(n))?
        .get("category")?
        .as_u64()?;
    ws.get("categories")?
        .as_array()?
        .iter()
        .find(|c| c.get("id").and_then(Value::as_u64) == Some(category))?
        .get("name")?
        .as_str()
        .map(String::from)
}

/// `name` as a surname: one word of a name ([`is_word`]), kept readable
/// rather than refused. Letters, digits and apostrophes stay; spaces and any
/// other character become one hyphen (`side project` → `Side-project`);
/// what does not start with a letter loses its lead (`3D prints` →
/// `D-prints`); the first letter is capitalised, the rest kept as written.
/// `None` when no letter is left.
pub fn as_surname(name: &str) -> Option<String> {
    let mut out = String::new();
    for c in name.trim().chars() {
        let c = if c.is_alphanumeric() || c == '\'' {
            c
        } else {
            '-'
        };
        if out.is_empty() && !c.is_alphabetic() {
            continue;
        }
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let mut out: String = out.chars().take(40).collect();
    while out.ends_with(['-', '\'']) {
        out.pop();
    }
    let mut chars = out.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
}

/// `<feeds>/names`.
pub fn dir(feeds: &Path) -> PathBuf {
    feeds.join(DIR)
}

/// Are sessions named, and their names shown? A `stat`.
pub fn is_on(feeds: &Path) -> bool {
    dir(feeds).join(ON).is_file()
}

/// Follow the switch: write or remove `<feeds>/names/on`. Returns whether
/// anything changed.
pub fn set_on(feeds: &Path, on: bool) -> std::io::Result<bool> {
    let flag = dir(feeds).join(ON);
    if on == flag.is_file() {
        return Ok(false);
    }
    if on {
        std::fs::create_dir_all(dir(feeds))?;
        std::fs::write(&flag, "")?;
    } else {
        std::fs::remove_file(&flag)?;
    }
    Ok(true)
}

/// An id usable as a file name.
fn safe(id: &str) -> bool {
    !id.is_empty() && !id.contains(['/', '\\']) && !id.starts_with('.')
}

fn session_file(feeds: &Path, session: &str) -> Option<PathBuf> {
    safe(session).then(|| dir(feeds).join(format!("{session}.json")))
}

fn asked_file(feeds: &Path, session: &str) -> Option<PathBuf> {
    safe(session).then(|| dir(feeds).join(ASKED).join(session))
}

fn tab_file(feeds: &Path, tab: &str) -> Option<PathBuf> {
    safe(tab).then(|| dir(feeds).join(TABS).join(tab))
}

fn root_file(feeds: &Path, root: &str) -> Option<PathBuf> {
    safe(root).then(|| dir(feeds).join("roots").join(format!("{root}.json")))
}

/// The `name` a names document at `file` holds.
fn name_in(file: &Path) -> Option<String> {
    doc_name(&read_doc(file)?)
}

/// A names document, parsed.
fn read_doc(file: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
}

/// The `name` a names document holds.
fn doc_name(doc: &Value) -> Option<String> {
    doc.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(String::from)
}

/// The `room` a names document holds.
fn doc_room(doc: &Value) -> Option<String> {
    doc.get("room")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(String::from)
}

fn write_doc(file: &Path, doc: &Value) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{doc}\n"))?;
    std::fs::rename(&tmp, file)
}

/// The names lock: held, every read of what
/// others hold and every rewrite of a names document is one step, so two
/// sessions starting at once draw in turn, and a room is drawn and written
/// once. Released when dropped. Best effort: where the file system takes
/// no lock (a share), the step runs unlocked, as it always did.
#[must_use]
struct Lock(#[allow(dead_code)] Option<std::fs::File>);

fn lock(feeds: &Path) -> Lock {
    let _ = std::fs::create_dir_all(dir(feeds));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir(feeds).join(LOCK))
        .ok()
        .filter(|f| f.lock().is_ok());
    Lock(file)
}

/// The name recorded for `session` itself: one small file read.
pub fn read(feeds: &Path, session: &str) -> Option<String> {
    name_in(&session_file(feeds, session)?)
}

/// `session`'s name: its own, else the one recorded for the conversation its
/// transcript holds (a resumed session's new id), which is then
/// recorded under the new id too, so the next look is one read again. Not
/// the conversation's name when another live session took its first name
/// while this one was gone: then none, and the session is
/// drawn a new one where it is named.
pub fn find(feeds: &Path, session: &str, transcript: Option<&Path>) -> Option<String> {
    find_among(feeds, session, transcript, || live_sessions(None))
}

/// [`find`], asking `live` for the live sessions only when it takes a
/// conversation's name.
pub fn find_among(
    feeds: &Path,
    session: &str,
    transcript: Option<&Path>,
    live: impl FnOnce() -> Live,
) -> Option<String> {
    if let Some(name) = read(feeds, session) {
        return Some(name);
    }
    let root = crate::subagents::conversation_root(transcript?)?;
    let doc = read_doc(&root_file(feeds, &root)?)?;
    let name = doc_name(&doc)?;
    let live = live();
    // Checked and taken in one step.
    let _lock = lock(feeds);
    if let Some(name) = read(feeds, session) {
        return Some(name);
    }
    if held_by(&holders(feeds, &live, session, Some(&root)), &name).is_some() {
        return None;
    }
    if let Some(file) = session_file(feeds, session) {
        let mut copy = json!({"session": session, "name": name, "root": root});
        // Its room comes with it: the same conversation, the
        // same place.
        if let Some(room) = doc_room(&doc) {
            copy["room"] = json!(room);
        }
        let _ = write_doc(&file, &copy);
    }
    Some(name)
}

/// The `name` on `session`'s manager feed: one named before names had their
/// own home. One read, no search.
fn on_feed(feeds: &Path, session: &str) -> Option<String> {
    crate::feed::read(&crate::feed::feed_path(feeds, session))?
        .name
        .filter(|n| !n.trim().is_empty())
}

/// Stamp `session`'s conversation root into its name, once its `transcript`
/// has one: a session named at its start has no turn yet, so
/// no root, and without one a resume (a new id, the same conversation)
/// would not find the name ([`find`]). Writes the root's file too. Returns
/// the name recorded, if any; a session already stamped costs one read.
pub fn keep_root(feeds: &Path, session: &str, transcript: Option<&Path>) -> Option<String> {
    let file = session_file(feeds, session)?;
    let doc = read_doc(&file)?;
    let name = doc_name(&doc)?;
    let unstamped = |doc: &Value| doc.get("root").and_then(Value::as_str).is_none();
    if unstamped(&doc)
        && let Some(root) = transcript.and_then(crate::subagents::conversation_root)
    {
        // Re-read under the lock: a room drawn meanwhile ([`room`]) stays.
        let _lock = lock(feeds);
        let Some(mut doc) = read_doc(&file).filter(unstamped) else {
            return Some(name);
        };
        doc["root"] = json!(root);
        let _ = write_doc(&file, &doc);
        if let Some(rf) = root_file(feeds, &root) {
            let _ = write_doc(&rf, &doc);
        }
    }
    Some(name)
}

/// The name the status line shows for `session`: none while the switch is
/// off; else its name ([`find`], its root stamped in once known,
/// [`keep_root`]), else the `name` on its manager feed ([`on_feed`]).
pub fn shown(feeds: &Path, session: &str, transcript: Option<&Path>) -> Option<String> {
    if !is_on(feeds) {
        return None;
    }
    keep_root(feeds, session, transcript)
        .or_else(|| on_feed(feeds, session))
        .or_else(|| find(feeds, session, transcript))
}

/// `session`'s room: the one recorded beside its name, else
/// its conversation's (another id of it, by the root its document holds),
/// else, for a document from before rooms, one drawn now and recorded under
/// both, so it is drawn once. None for a session with no name recorded.
pub fn room(feeds: &Path, session: &str) -> Option<String> {
    let file = session_file(feeds, session)?;
    let mut doc = read_doc(&file)?;
    doc_name(&doc)?;
    if let Some(room) = doc_room(&doc) {
        return Some(room);
    }
    // Drawn and written in one step: two looks at once, or a
    // root stamped meanwhile ([`keep_root`]), draw it once.
    let _lock = lock(feeds);
    doc = read_doc(&file)?;
    if let Some(room) = doc_room(&doc) {
        return Some(room);
    }
    let root = doc.get("root").and_then(Value::as_str).map(String::from);
    let rf = root.as_deref().and_then(|r| root_file(feeds, r));
    let mut root_doc = rf.as_deref().and_then(read_doc);
    let room = root_doc
        .as_ref()
        .and_then(doc_room)
        .unwrap_or_else(draw_room);
    doc["room"] = json!(room);
    let _ = write_doc(&file, &doc);
    if let (Some(rf), Some(rd)) = (rf, root_doc.as_mut())
        && doc_room(rd).is_none()
    {
        rd["room"] = json!(room);
        let _ = write_doc(&rf, rd);
    }
    Some(room)
}

/// The place a named `session`'s tab is titled after:
/// `Rotem's Workshop`, its first name and its [`room`]. None while the
/// switch is off ([`is_on`]) or the session has no name recorded under its
/// id: its tab keeps Claude's own title.
pub fn place(feeds: &Path, session: &str) -> Option<String> {
    if !is_on(feeds) {
        return None;
    }
    let name = read(feeds, session).or_else(|| adopt(feeds, session))?;
    place_of(&name, &room(feeds, session)?)
}

/// Give `session`, named only on its manager feed (from before names had a
/// home), a names document of that name, so it has a room to
/// be titled after. Nothing writes a feed's name any more, so
/// the copy never goes stale; the status line stamps its root in
/// ([`keep_root`]).
fn adopt(feeds: &Path, session: &str) -> Option<String> {
    let name = on_feed(feeds, session)?;
    let file = session_file(feeds, session)?;
    let _lock = lock(feeds);
    if let Some(name) = name_in(&file) {
        return Some(name);
    }
    write_doc(
        &file,
        &json!({"session": session, "name": name, "room": draw_room()}),
    )
    .ok()?;
    Some(name)
}

/// One word of a name: a letter, then letters, digits, hyphens and
/// apostrophes; 40 characters at most.
fn is_word(w: &str) -> bool {
    let mut chars = w.chars();
    chars.next().is_some_and(char::is_alphabetic)
        && chars.all(|c| c.is_alphanumeric() || c == '-' || c == '\'')
        && w.chars().count() <= 40
}

/// The full name `given` makes: two words are kept as given; one word keeps
/// `previous` when its first name is the same (case aside), else takes the
/// session's surname, `surname()` ([`surname`]).
pub fn full_name(
    given: &str,
    previous: Option<&str>,
    surname: impl FnOnce() -> String,
) -> Result<String, String> {
    let words: Vec<&str> = given.split_whitespace().collect();
    let bad = || {
        format!(
            "a name is a first name, or a first name and a surname \
             (letters, digits, hyphens, apostrophes): got {given:?}"
        )
    };
    match words.as_slice() {
        [first, last] if is_word(first) && is_word(last) => Ok(format!("{first} {last}")),
        [first] if is_word(first) => {
            // The same first name, case aside: the name stands as recorded.
            let kept = previous.filter(|p| {
                let mut w = p.split_whitespace();
                w.next()
                    .is_some_and(|pf| pf.to_lowercase() == first.to_lowercase())
                    && w.next().is_some()
            });
            Ok(match kept {
                Some(p) => p.split_whitespace().collect::<Vec<_>>().join(" "),
                None => format!("{first} {}", surname()),
            })
        }
        _ => Err(bad()),
    }
}

/// One of `list`, at random: the std hasher's random keys over the clock and
/// the process id, enough to spread names and nothing to build a crate on.
fn pick(list: &[&str]) -> String {
    use std::hash::BuildHasher;
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = std::collections::hash_map::RandomState::new().hash_one((seed, std::process::id()));
    list[(n % list.len() as u64) as usize].to_string()
}

/// A first name from [`FIRST_NAMES`], at random.
pub fn draw_first_name() -> String {
    pick(FIRST_NAMES)
}

/// The sessions live now, each with the directory its claude runs in.
pub type Live = std::collections::HashMap<String, PathBuf>;

/// The sessions whose claude runs now: Claude Code's own registry
/// (`sessions/<pid>.json`, pid-checked, [`crate::registry::scan`]) in every
/// account on the machine, `cfg` first when given. What the panel and the
/// reach guard call live; a session gone is gone from it, whatever its
/// files say.
pub fn live_sessions(cfg: Option<&Path>) -> Live {
    crate::registry::scan(crate::registry::ambient_config_dirs(cfg))
        .into_iter()
        .map(|s| (s.entry.session_id, s.entry.cwd))
        .collect()
}

/// A live session and the name it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub session: String,
    pub name: String,
    /// Where its claude runs: the repo, or the worktree its task is in.
    pub cwd: PathBuf,
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix: String = self.session.chars().take(8).collect();
        write!(f, "{} (session {prefix}", self.name)?;
        if !self.cwd.as_os_str().is_empty() {
            let cwd = match dirs::home_dir().and_then(|h| self.cwd.strip_prefix(h).ok()) {
                Some(rel) => Path::new("~").join(rel),
                None => self.cwd.clone(),
            };
            write!(f, ", in {}", cwd.display())?;
        }
        write!(f, ")")
    }
}

/// Every `live` session but `session` itself that holds a name: its own
/// ([`DIR`]), else the one on its manager feed. A session of the same
/// conversation (`root`: a resume's other id, a job's tab) is `session`
/// itself under another id, so it holds nothing against it.
fn holders(feeds: &Path, live: &Live, session: &str, root: Option<&str>) -> Vec<Holder> {
    let mut out: Vec<Holder> = live
        .iter()
        .filter(|(id, _)| id.as_str() != session)
        .filter_map(|(id, cwd)| {
            let doc = session_file(feeds, id).and_then(|f| read_doc(&f));
            let theirs = doc.as_ref().and_then(|d| d.get("root")?.as_str());
            if root.is_some() && theirs == root {
                return None;
            }
            let name = doc
                .as_ref()
                .and_then(doc_name)
                .or_else(|| on_feed(feeds, id))?;
            Some(Holder {
                session: id.clone(),
                name,
                cwd: cwd.clone(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.session.cmp(&b.session));
    out
}

/// The sessions not in `live` whose names document was written in the
/// last `within`: just named, and live by now though
/// Claude Code's registry may not list them yet. `session` itself and
/// another id of its conversation (`root`) hold nothing against it. What a
/// draw steers clear of beside [`holders`]; a name given by hand is checked
/// against the live ones alone.
fn fresh(
    feeds: &Path,
    live: &Live,
    session: &str,
    root: Option<&str>,
    within: std::time::Duration,
) -> Vec<Holder> {
    let now = std::time::SystemTime::now();
    let Ok(entries) = std::fs::read_dir(dir(feeds)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let id = path.file_stem()?.to_str()?.to_string();
            if path.extension()? != "json" || id == session || live.contains_key(&id) {
                return None;
            }
            let age = now.duration_since(e.metadata().ok()?.modified().ok()?);
            if age.is_ok_and(|a| a > within) {
                return None;
            }
            let doc = read_doc(&path)?;
            if root.is_some() && doc.get("root").and_then(Value::as_str) == root {
                return None;
            }
            Some(Holder {
                session: id,
                name: doc_name(&doc)?,
                cwd: PathBuf::new(),
            })
        })
        .collect()
}

/// A name's first name, for comparing: case aside.
fn first_of(name: &str) -> Option<String> {
    name.split_whitespace().next().map(str::to_lowercase)
}

/// The holder whose first name is `name`'s, if any.
fn held_by<'a>(held: &'a [Holder], name: &str) -> Option<&'a Holder> {
    let first = first_of(name)?;
    held.iter()
        .find(|h| first_of(&h.name).as_deref() == Some(first.as_str()))
}

/// A full name for a session that has none: a first name no
/// session in `held` holds (drawn from the pool less theirs,
/// not hoped for), and its `surname` ([`surname`]). Every first
/// name held (hundreds of live sessions) draws from the whole pool. Two
/// sessions may share a full name over time (a second Rotem Giverny once the
/// first has ended); no two live ones share a first name.
pub fn draw_name(held: &[Holder], surname: &str) -> String {
    let free: Vec<&str> = FIRST_NAMES
        .iter()
        .copied()
        .filter(|f| held_by(held, f).is_none())
        .collect();
    let pool: &[&str] = if free.is_empty() { FIRST_NAMES } else { &free };
    format!("{} {surname}", pick(pool))
}

/// Record `session`'s name from `given` ([`full_name`]), under its id and,
/// when known, its conversation's `root`. Returns the full name and the one
/// it replaced. Refused when another live session ([`live_sessions`]) holds
/// its first name, naming who; `session` re-taking its own,
/// under this id or another of its conversation, is fine.
pub fn record(
    feeds: &Path,
    session: &str,
    given: &str,
    root: Option<&str>,
    surname: impl FnOnce() -> String,
) -> Result<(String, Option<String>), String> {
    record_among(feeds, session, given, root, surname, &live_sessions(None))
}

/// [`record`] against the `live` sessions given.
pub fn record_among(
    feeds: &Path,
    session: &str,
    given: &str,
    root: Option<&str>,
    surname: impl FnOnce() -> String,
    live: &Live,
) -> Result<(String, Option<String>), String> {
    // Who holds what, read and written in one step.
    let _lock = lock(feeds);
    record_held(
        feeds,
        session,
        given,
        root,
        surname,
        &holders(feeds, live, session, root),
    )
}

/// [`record`], refused when a session in `held` holds the first name.
fn record_held(
    feeds: &Path,
    session: &str,
    given: &str,
    root: Option<&str>,
    surname: impl FnOnce() -> String,
    held: &[Holder],
) -> Result<(String, Option<String>), String> {
    let file =
        session_file(feeds, session).ok_or_else(|| format!("not a session id: {session:?}"))?;
    let docs = [
        read_doc(&file),
        root.and_then(|r| read_doc(&root_file(feeds, r)?)),
    ];
    let previous = docs.iter().flatten().find_map(doc_name);
    let name = full_name(given, previous.as_deref(), surname)?;
    if let Some(h) = held_by(held, &name) {
        let first = name.split_whitespace().next().unwrap_or(&name);
        return Err(format!(
            "{first} is taken: {h} holds it, and no two live sessions share a first name; \
             give another, or run `giverny-manage name` alone to be drawn one"
        ));
    }
    // The room is drawn at birth and kept through any renaming:
    // the tab's title is the session's place, not its name
    // of the moment.
    let room = docs
        .iter()
        .flatten()
        .find_map(doc_room)
        .unwrap_or_else(draw_room);
    let mut doc = json!({"session": session, "name": name, "room": room});
    if let Some(r) = root {
        doc["root"] = json!(r);
    }
    write_doc(&file, &doc).map_err(|e| format!("{}: {e}", file.display()))?;
    if let Some(rf) = root.and_then(|r| root_file(feeds, r)) {
        write_doc(&rf, &doc).map_err(|e| format!("{}: {e}", rf.display()))?;
    }
    Ok((name, previous))
}

/// Draw `session` a first name and record it with its `surname`
/// ([`draw_name`]), under its conversation's root when its `transcript` has
/// one: what a session with no name gets, from `giverny-manage name` or its
/// start.
pub fn draw_and_record(
    feeds: &Path,
    session: &str,
    transcript: Option<&Path>,
    surname: &str,
) -> Result<String, String> {
    draw_and_record_among(feeds, session, transcript, surname, &live_sessions(None))
}

/// [`draw_and_record`] against the `live` sessions given: the first name
/// drawn is none of theirs.
pub fn draw_and_record_among(
    feeds: &Path,
    session: &str,
    transcript: Option<&Path>,
    surname: &str,
    live: &Live,
) -> Result<String, String> {
    let root = transcript.and_then(crate::subagents::conversation_root);
    // Drawn and recorded in one step: a session drawing at
    // the same instant draws after, and sees this one's name.
    let _lock = lock(feeds);
    let mut held = holders(feeds, live, session, root.as_deref());
    held.extend(fresh(feeds, live, session, root.as_deref(), FRESH));
    // A full name: `record` keeps it as given and adds nothing. Drawn
    // clear of `held` already, so checked against nobody: with every first
    // name held, the draw from the whole pool still stands.
    record_held(
        feeds,
        session,
        &draw_name(&held, surname),
        root.as_deref(),
        || surname.to_string(),
        &[],
    )
    .map(|(name, _)| name)
}

/// What a session in a Giverny tab is told at its start: its name (drawn
/// there unless it had one), else, only when recording it
/// failed, to ask Giverny for one (Giverny draws the whole
/// name, nothing is picked).
pub fn ask(name: Option<&str>) -> String {
    match name {
        Some(n) => format!(
            "Your name in this session is {n} (Giverny shows it on the status line; \
             `giverny-manage name` prints it)."
        ),
        None => "Giverny names its sessions: run `giverny-manage name` and it gives you your \
                 name, a first name and a surname; run it again any time to read it back."
            .to_string(),
    }
}

/// The session a hook payload is for, as a usable id.
fn session_of(payload: &Value) -> Option<&str> {
    payload
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| safe(s))
}

/// The working directory a hook payload names.
fn cwd_of(payload: &Value) -> Option<PathBuf> {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
}

/// The transcript a hook payload names.
fn transcript_of(payload: &Value) -> Option<PathBuf> {
    payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(PathBuf::from)
}

/// The name, and room, the session a `/clear` in `tab` cleared held, taken
/// back by `session`, the one that `/clear` started: the same agent to the
/// person, under a new id. Recorded under `session`, its root stamped in
/// later as any start's ([`keep_root`]). None, and the session is drawn a
/// name as ever, when the tab has no session told before it, that one has
/// no name, or a live session other than the cleared one holds its first
/// name meanwhile. The cleared session itself holds nothing against it,
/// though Claude Code's registry may list it still, or its names document
/// be fresh ([`FRESH`]).
fn after_clear(feeds: &Path, session: &str, tab: &str, live: &Live) -> Option<String> {
    let cleared = std::fs::read_to_string(tab_file(feeds, tab)?).ok()?;
    let cleared = cleared.trim();
    if cleared == session || !safe(cleared) {
        return None;
    }
    let file = session_file(feeds, session)?;
    // Checked and taken in one step.
    let _lock = lock(feeds);
    let doc = read_doc(&session_file(feeds, cleared)?)?;
    let name = doc_name(&doc)?;
    let mut held = holders(feeds, live, session, None);
    held.extend(fresh(feeds, live, session, None, FRESH));
    held.retain(|h| h.session != cleared);
    if held_by(&held, &name).is_some() {
        return None;
    }
    let mut taken = json!({"session": session, "name": name});
    if let Some(room) = doc_room(&doc) {
        taken["room"] = json!(room);
    }
    write_doc(&file, &taken)
        .inspect_err(|e| eprintln!("giverny hook: naming {session}: {e}"))
        .ok()?;
    Some(name)
}

/// Note `session` as the one `tab` holds now, for a `/clear` there to find
/// ([`after_clear`]).
fn keep_tab(feeds: &Path, tab: &str, session: &str) {
    if let Some(file) = tab_file(feeds, tab) {
        let _ = std::fs::create_dir_all(dir(feeds).join(TABS));
        let _ = std::fs::write(file, session);
    }
}

/// Name `session` and say so ([`ask`]): the name it has ([`find`], else
/// the one on its manager feed), else, started by a `/clear`, the name of
/// the session it cleared in its tab ([`after_clear`]), else one drawn and
/// recorded right here ([`draw_and_record`]) with the surname of the tab it
/// was born in ([`surname`]), so the session has its name before its first
/// turn. Marked as told, so it is not told again ([`ASKED`]), and noted as
/// its tab's ([`keep_tab`]).
fn tell(payload: &Value, feeds: &Path, session: &str) -> String {
    let transcript = transcript_of(payload);
    let tab = crate::lineage::giverny_var("GIVERNY_TAB_ID");
    let tab = tab.as_deref().map(str::trim);
    let cleared = payload.get("source").and_then(Value::as_str) == Some("clear");
    let name = find(feeds, session, transcript.as_deref())
        .or_else(|| on_feed(feeds, session))
        .or_else(|| {
            after_clear(
                feeds,
                session,
                tab.filter(|_| cleared)?,
                &live_sessions(None),
            )
        })
        .or_else(|| {
            let born = surname(feeds, cwd_of(payload).as_deref());
            draw_and_record(feeds, session, transcript.as_deref(), &born)
                .inspect_err(|e| eprintln!("giverny hook: naming {session}: {e}"))
                .ok()
        });
    if let Some(file) = asked_file(feeds, session) {
        let _ = std::fs::create_dir_all(dir(feeds).join(ASKED));
        let _ = std::fs::write(file, "");
    }
    if let Some(tab) = tab.filter(|_| name.is_some()) {
        keep_tab(feeds, tab, session);
    }
    ask(name.as_deref())
}

/// The `SessionStart` hook's reply: [`start_context`] as
/// `additionalContext`.
pub fn session_start(payload: &Value, feeds: &Path, on: bool, in_tab: bool) -> Option<String> {
    let context = start_context(payload, feeds, on, in_tab)?;
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": context
            }
        })
        .to_string(),
    )
}

/// What a session start is told, naming it first ([`tell`]): only with the
/// switch `on`, only for a top-level session (`in_tab`: a Giverny tab's own,
/// not a background job's or a nested claude's), never a subagent's. Every
/// start tells again (a `/clear` is a new session but keeps the name the
/// cleared one had in its tab; a resume or a compaction keeps its name),
/// and is marked so the prompt hook stays quiet ([`first_prompt`]). The
/// text alone, for a hook reply that carries more.
pub fn start_context(payload: &Value, feeds: &Path, on: bool, in_tab: bool) -> Option<String> {
    if !on || !in_tab || payload.get("agent_id").is_some() {
        return None;
    }
    let session = session_of(payload)?;
    Some(tell(payload, feeds, session))
}

/// The naming a `UserPromptSubmit` carries when the session's start did
/// not: its claude predates the plugin's `SessionStart` hook, or the switch
/// was off then. It names the session as its start would have
/// ([`tell`]). The same gates as [`session_start`], plus once
/// per session; `in_tab` is asked only for a session not yet told, as it
/// walks the process tree. A session told already has its root stamped in
/// instead, once its transcript has one ([`keep_root`]). Returns the context
/// text, for the prompt hook's reply.
pub fn first_prompt(
    payload: &Value,
    feeds: &Path,
    on: bool,
    in_tab: impl FnOnce() -> bool,
) -> Option<String> {
    if !on || payload.get("agent_id").is_some() {
        return None;
    }
    let session = session_of(payload)?;
    if asked_file(feeds, session)?.exists() {
        keep_root(feeds, session, transcript_of(payload).as_deref());
        return None;
    }
    if !in_tab() {
        return None;
    }
    Some(tell(payload, feeds, session))
}

/// A `SessionStart` hook payload, parsed; `None` for any other event (read
/// whole only when it names that one: the others can be large).
pub fn start_payload(input: &str) -> Option<Value> {
    if !input.contains("\"SessionStart\"") {
        return None;
    }
    let v: Value = serde_json::from_str(input).ok()?;
    (v.get("hook_event_name").and_then(Value::as_str) == Some("SessionStart")).then_some(v)
}

/// A `SessionEnd` hook payload, parsed; `None` for any other event.
pub fn end_payload(input: &str) -> Option<Value> {
    if !input.contains("\"SessionEnd\"") {
        return None;
    }
    let v: Value = serde_json::from_str(input).ok()?;
    (v.get("hook_event_name").and_then(Value::as_str) == Some("SessionEnd")).then_some(v)
}

/// How much of a transcript's end is read for its titles: what Claude Code
/// reads for a `/resume` row (64 KiB on 2.1.312), so a title of ours it
/// would no longer see there is one we no longer see either, and append
/// again.
const TITLE_TAIL: u64 = 64 * 1024;

/// The most characters of the session's own text a title carries; the
/// picker cuts a row to the terminal's width anyway.
const TITLE_TEXT: usize = 120;

/// Between the name and the session's own text in its title.
pub const TITLE_SEP: &str = " · ";

/// What a transcript's tail says about its title: the last line of each
/// kind Claude Code writes.
#[derive(Debug, Default, PartialEq)]
struct Titles {
    /// The last `custom-title` (`/rename`, `--name`, or ours).
    custom: Option<String>,
    /// An `agent-name` line: the session was started with `--name`.
    agent: bool,
    /// The last `ai-title`, Claude Code's own title for it.
    ai: Option<String>,
    /// The last `summary` (older transcripts).
    summary: Option<String>,
    /// The last `last-prompt`.
    prompt: Option<String>,
}

/// The title lines in a transcript's `tail`. Its first line may be cut and
/// is then skipped, as any line that does not parse.
fn titles_in(tail: &str) -> Titles {
    let mut t = Titles::default();
    let field = |line: &str, key: &str| -> Option<String> {
        let v: Value = serde_json::from_str(line).ok()?;
        v.get(key).and_then(Value::as_str).map(String::from)
    };
    for line in tail.lines() {
        let Some(kind) = line
            .strip_prefix("{\"type\":\"")
            .and_then(|r| r.split('"').next())
        else {
            continue;
        };
        match kind {
            "custom-title" => t.custom = field(line, "customTitle").or(t.custom),
            "agent-name" => t.agent = true,
            "ai-title" => t.ai = field(line, "aiTitle").or(t.ai),
            "summary" => t.summary = field(line, "summary").or(t.summary),
            "last-prompt" => t.prompt = field(line, "lastPrompt").or(t.prompt),
            _ => {}
        }
    }
    t
}

/// The text Claude Code's `/resume` picker shows for a session with no
/// custom title: its AI title, else a summary, else its last prompt (2.1.312
/// picks `agentName || customTitle || aiTitle || summary || prompt`, the
/// prompt read from the tail's `last-prompt`). On one line, cut to
/// [`TITLE_TEXT`] characters.
fn usual_text(t: &Titles) -> Option<String> {
    let text = [&t.ai, &t.summary, &t.prompt]
        .into_iter()
        .flatten()
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|s| !s.is_empty())?;
    Some(if text.chars().count() > TITLE_TEXT {
        text.chars().take(TITLE_TEXT - 1).collect::<String>() + "…"
    } else {
        text
    })
}

/// The custom title a session named `name` should be given, from its
/// transcript's `tail`, or none when it needs no new one: its
/// name and the text the picker would show without it,
/// `Rotem Giverny · Fix the login redirect`, so a `/resume` row says whose
/// it is and still what it was. None when that is the title already, when
/// there is no text yet (a session with no prompt is not listed), and when
/// the title is the person's: a `custom-title` not starting with the name
/// (`/rename`, `claude --name`) or an `agent-name` (`--name`) is left alone.
pub fn title_for(name: &str, tail: &str) -> Option<String> {
    let t = titles_in(tail);
    let ours = format!("{name}{TITLE_SEP}");
    if t.agent || t.custom.as_deref().is_some_and(|c| !c.starts_with(&ours)) {
        return None;
    }
    let title = format!("{ours}{}", usual_text(&t)?);
    (t.custom.as_deref() != Some(title.as_str())).then_some(title)
}

/// The last [`TITLE_TAIL`] bytes of `transcript`, or none when it does not
/// end a line: Claude Code is writing one, and a line of ours now would be
/// glued to it.
fn transcript_tail(transcript: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TITLE_TAIL)))
        .ok()?;
    let mut buf = Vec::new();
    file.take(TITLE_TAIL).read_to_end(&mut buf).ok()?;
    (buf.last() == Some(&b'\n')).then(|| String::from_utf8_lossy(&buf).into_owned())
}

/// Keep a named session's custom title its name and its usual text
/// ([`title_for`]), from its `SessionStart`, `Stop` and
/// `SessionEnd` hooks: the text appears with the first prompt, Claude
/// Code's AI title can move later, and what Claude Code writes after a
/// turn's `Stop` (the first turn's prompt snapshot is ~90 KB) can push the
/// title out of the [`TITLE_TAIL`] the picker reads, so it is appended
/// again at the end. One `custom-title` line appended to the transcript, in one
/// `O_APPEND` write, as `/rename` writes it; the file is never rewritten.
/// The running session does not take it up (2.1.312); a resumed one loads
/// it as its title and re-appends it itself. Only for a session with a
/// name, never a subagent's. Returns the title
/// written.
pub fn keep_title(feeds: &Path, payload: &Value) -> Option<String> {
    use std::io::Write;
    if payload.get("agent_id").is_some() {
        return None;
    }
    let session = session_of(payload)?;
    let transcript = transcript_of(payload)?;
    let name = find(feeds, session, Some(&transcript))?;
    let title = title_for(&name, &transcript_tail(&transcript)?)?;
    let line = format!(
        "{{\"type\":\"custom-title\",\"customTitle\":{},\"sessionId\":{}}}\n",
        Value::from(title.as_str()),
        Value::from(session)
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .ok()?;
    file.write_all(line.as_bytes())
        .inspect_err(|e| eprintln!("giverny hook: titling {session}: {e}"))
        .ok()?;
    Some(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giverny-names-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn drawn(s: &str) -> impl FnOnce() -> String + '_ {
        move || s.to_string()
    }

    #[test]
    fn a_first_name_gets_a_drawn_surname_once_and_keeps_it() {
        let feeds = scratch("draw");
        let (n, was) = record(&feeds, "s1", "Tamsin", Some("r1"), drawn("Camden")).unwrap();
        assert_eq!((n.as_str(), was), ("Tamsin Camden", None));
        // The same first name again: the surname stands, nothing is drawn.
        let (n, was) = record(&feeds, "s1", "tamsin", Some("r1"), || panic!("no draw")).unwrap();
        assert_eq!(n, "Tamsin Camden");
        assert_eq!(was.as_deref(), Some("Tamsin Camden"));
        // Another first name is another person: a new surname.
        let (n, _) = record(&feeds, "s1", "Wren", Some("r1"), drawn("Ashby")).unwrap();
        assert_eq!(n, "Wren Ashby");
        assert_eq!(read(&feeds, "s1").as_deref(), Some("Wren Ashby"));
        let _ = std::fs::remove_dir_all(&feeds);
    }

    /// A transcript line of `kind` carrying `key: value`, as Claude Code
    /// writes it.
    fn meta(kind: &str, key: &str, value: &str) -> String {
        format!(
            "{{\"type\":\"{kind}\",\"{key}\":{},\"sessionId\":\"s\"}}\n",
            Value::from(value)
        )
    }

    #[test]
    fn a_clear_takes_back_the_name_the_tab_had() {
        let feeds = scratch("clear");
        let live = |ids: &[&str]| -> Live {
            ids.iter()
                .map(|i| (i.to_string(), PathBuf::new()))
                .collect()
        };
        // No session told in the tab yet: nothing to take back.
        assert_eq!(after_clear(&feeds, "s2", "giverny-7", &live(&[])), None);
        record_among(
            &feeds,
            "s1",
            "Rotem Giverny",
            Some("r1"),
            drawn("x"),
            &live(&[]),
        )
        .unwrap();
        let kept = room(&feeds, "s1").unwrap();
        keep_tab(&feeds, "giverny-7", "s1");
        // The registry may still list the cleared session, its document
        // fresh: it holds nothing against the session that follows it.
        let name = after_clear(&feeds, "s2", "giverny-7", &live(&["s1", "s2"]));
        assert_eq!(name.as_deref(), Some("Rotem Giverny"));
        assert_eq!(read(&feeds, "s2").as_deref(), Some("Rotem Giverny"));
        assert_eq!(room(&feeds, "s2"), Some(kept));
        // Another tab's session took the first name meanwhile: drawn anew.
        let _ = std::fs::remove_file(session_file(&feeds, "s2").unwrap());
        record_among(&feeds, "s9", "Rotem Ashby", None, drawn("x"), &live(&[])).unwrap();
        assert_eq!(after_clear(&feeds, "s2", "giverny-7", &live(&["s9"])), None);
        let _ = std::fs::remove_dir_all(&feeds);
    }

    #[test]
    fn a_title_the_person_gave_is_left_alone() {
        let text = meta("ai-title", "aiTitle", "Login redirect fix");
        // `/rename`.
        let renamed = text.clone() + &meta("custom-title", "customTitle", "auth work");
        assert_eq!(title_for("Rotem Giverny", &renamed), None);
        // `/rename` over a title of ours: the last one counts.
        let over = meta(
            "custom-title",
            "customTitle",
            "Rotem Giverny · Login redirect fix",
        ) + &renamed;
        assert_eq!(title_for("Rotem Giverny", &over), None);
        // `claude --name`, which writes an agent name too.
        let named = text.clone()
            + &meta("custom-title", "customTitle", "g297-send")
            + &meta("agent-name", "agentName", "g297-send");
        assert_eq!(title_for("Rotem Giverny", &named), None);
        // Someone else's name is not ours either.
        let other = text
            + &meta(
                "custom-title",
                "customTitle",
                "Fenn Giverny · Login redirect fix",
            );
        assert_eq!(title_for("Rotem Giverny", &other), None);
    }
}
