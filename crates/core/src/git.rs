//! Lightweight git info: branch name via `.git/HEAD` parsing.
//!
//! No libgit2/gix dependency — a HEAD read is enough for display, is
//! worktree-aware (`.git` file → `gitdir:` indirection), and walks up from
//! the cwd to find the repository root.

use std::path::{Path, PathBuf};

/// Branch (or short detached sha) for the repository containing `dir`.
pub fn branch_of(dir: &Path) -> Option<String> {
    let git_path = find_git(dir)?;
    let head = std::fs::read_to_string(git_path.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        return Some(name.to_string());
    }
    // Detached HEAD: short sha.
    if head.len() >= 8 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(format!("@{}", &head[..8]));
    }
    None
}

/// The working tree `dir` belongs to: the nearest ancestor holding a `.git`.
///
/// The repository as a *place*, which is what grouping tabs by repository
/// needs — `find_git` answers with the git dir, and for a worktree or a
/// submodule that is somewhere else entirely.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Resolve the git dir for `dir`, walking up ancestors; follows a `.git`
/// *file*'s `gitdir:` pointer (worktrees, submodules).
fn find_git(dir: &Path) -> Option<PathBuf> {
    for ancestor in dir.ancestors() {
        let dotgit = ancestor.join(".git");
        if dotgit.is_dir() {
            return Some(dotgit);
        }
        if dotgit.is_file() {
            let content = std::fs::read_to_string(&dotgit).ok()?;
            let target = content.trim().strip_prefix("gitdir: ")?.trim();
            let path = Path::new(target);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                ancestor.join(path)
            };
            return Some(resolved);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_root_is_the_working_tree() {
        let root = scratch("root");
        let deep = root.join("crates/app/src");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(repo_root(&deep), None, "no repository yet");

        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(repo_root(&deep).as_deref(), Some(root.as_path()));
        assert_eq!(repo_root(&root).as_deref(), Some(root.as_path()));

        // A worktree keeps its git dir elsewhere and a `.git` file here; the
        // place is still this directory.
        let wt = scratch("worktree");
        std::fs::write(wt.join(".git"), "gitdir: /somewhere/else\n").unwrap();
        assert_eq!(repo_root(&wt).as_deref(), Some(wt.as_path()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&wt);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("giverny-git-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn branch_from_head_ref() {
        let dir = scratch("branch");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feat/tea\n").unwrap();
        assert_eq!(branch_of(&dir).as_deref(), Some("feat/tea"));
        // From a subdirectory too (ancestor walk).
        let sub = dir.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(branch_of(&sub).as_deref(), Some("feat/tea"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detached_head_shows_short_sha() {
        let dir = scratch("detached");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(
            dir.join(".git/HEAD"),
            "0123abcd0123abcd0123abcd0123abcd0123abcd\n",
        )
        .unwrap();
        assert_eq!(branch_of(&dir).as_deref(), Some("@0123abcd"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_gitdir_file() {
        let dir = scratch("worktree");
        let real = dir.join("repo/.git/worktrees/wt1");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("HEAD"), "ref: refs/heads/wt-branch\n").unwrap();
        let wt = dir.join("wt1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", real.display())).unwrap();
        assert_eq!(branch_of(&wt).as_deref(), Some("wt-branch"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_repo_is_none() {
        let dir = scratch("plain");
        assert_eq!(branch_of(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
