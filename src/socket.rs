use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    let uid = nix::unistd::Uid::current().as_raw();
    PathBuf::from(format!("/tmp/.rexec-{uid}"))
}
