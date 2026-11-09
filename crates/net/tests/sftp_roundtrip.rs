//! Teste ponta a ponta do ator SFTP contra um servidor russh local.
// O lock serializa variáveis de ambiente globais; pode atravessar await aqui.
#![allow(clippy::await_holding_lock)]

use russh::server::{
    self, Auth, ChannelOpenHandle, Handle as SessionHandle, Msg, Server as _,
    Session as ServerSession,
};
use russh::{Channel, ChannelId, Disconnect};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, FileMode, Handle, Name, OpenFlags, Status, StatusCode,
    Version,
};
use sshc_core::Protocol;
use sshc_net::sftp::{
    local_dest, normalize_remote, ConflictDecision, ConflictPolicy, Direction, EntryKind,
    JobOutcome, SftpCmd, SftpConnection, SftpEvent, CHUNK_SIZE,
};
use sshc_net::Credentials;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const TEST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACC9mauQz3+ny7HSOfdhHx4vW+khzVHAx27y6eeyjLwhUgAAAJAV7SyBFe0s
gQAAAAtzc2gtZWQyNTUxOQAAACC9mauQz3+ny7HSOfdhHx4vW+khzVHAx27y6eeyjLwhUg
AAAEDM51WOxUe9g61Q9jDqvO3DVhUaC3D/nyCTeo7H9SpQ4r2Zq5DPf6fLsdI592EfHi9b
6SHNUcDHbvLp57KMvCFSAAAACXNzaGMtdGVzdAECAwQ=
-----END OPENSSH PRIVATE KEY-----
";

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn connect_lists_files() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
    std::fs::create_dir(dir.path().join("pasta")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert_eq!(conn.home.as_deref(), Some("/"));
    assert!(conn.send(SftpCmd::List {
        id: 1,
        path: "/".into()
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::Listed {
            id: 1,
            path,
            result,
        } => {
            assert_eq!(path, "/");
            let entries = result.unwrap();
            assert!(entries
                .iter()
                .any(|e| e.name == "hello.txt" && e.kind == EntryKind::File));
            assert!(entries
                .iter()
                .any(|e| e.name == "pasta" && e.kind == EntryKind::Dir));
        }
        other => panic!("esperava Listed, veio {other:?}"),
    }

    // "~" resolve para o home que o servidor informou (realpath "." = "/").
    assert!(conn.send(SftpCmd::List {
        id: 2,
        path: "~".into()
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::Listed {
            id: 2,
            path,
            result,
        } => {
            assert_eq!(path, "/");
            assert!(result
                .unwrap()
                .iter()
                .any(|e| e.name == "hello.txt" && e.kind == EntryKind::File));
        }
        other => panic!("esperava Listed, veio {other:?}"),
    }
}

#[tokio::test]
async fn mkdir_rename_delete_roundtrip() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("pasta")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    op(
        &conn,
        SftpCmd::Mkdir {
            id: 1,
            path: "/nova".into(),
        },
    )
    .await
    .unwrap();
    assert!(dir.path().join("nova").is_dir());

    op(
        &conn,
        SftpCmd::Rename {
            id: 2,
            from: "/nova".into(),
            to: "/velha".into(),
        },
    )
    .await
    .unwrap();
    assert!(dir.path().join("velha").is_dir());

    op(
        &conn,
        SftpCmd::Delete {
            id: 3,
            paths: vec!["/velha".into()],
            recursive: true,
        },
    )
    .await
    .unwrap();
    assert!(!dir.path().join("velha").exists());

    // Erro do servidor vira OpDone::Err com o motivo.
    let error = op(
        &conn,
        SftpCmd::Mkdir {
            id: 4,
            path: "/pasta".into(),
        },
    )
    .await
    .unwrap_err();
    assert!(error.contains("criando /pasta"), "erro inesperado: {error}");
    assert!(dir.path().join("pasta").is_dir());

    // Delete não recursivo de arquivo.
    std::fs::write(dir.path().join("solto.txt"), b"x").unwrap();
    op(
        &conn,
        SftpCmd::Delete {
            id: 5,
            paths: vec!["/solto.txt".into()],
            recursive: false,
        },
    )
    .await
    .unwrap();
    assert!(!dir.path().join("solto.txt").exists());
}

#[tokio::test]
async fn delete_recursive_removes_tree() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("arvore/sub")).unwrap();
    std::fs::write(dir.path().join("arvore/a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("arvore/sub/b.txt"), b"b").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    // Sem recursão, pasta não vazia falha e nada é removido.
    let error = op(
        &conn,
        SftpCmd::Delete {
            id: 1,
            paths: vec!["/arvore".into()],
            recursive: false,
        },
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("apagando /arvore"),
        "erro inesperado: {error}"
    );
    assert!(dir.path().join("arvore/sub/b.txt").exists());

    op(
        &conn,
        SftpCmd::Delete {
            id: 2,
            paths: vec!["/arvore".into()],
            recursive: true,
        },
    )
    .await
    .unwrap();
    assert!(!dir.path().join("arvore").exists());
}

#[tokio::test]
async fn delete_root_is_rejected() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sentinela.txt"), b"x").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    // "/.." normaliza para "/": nenhum dos dois pode esvaziar a raiz remota.
    for (id, path) in [(1u64, "/"), (2u64, "/..")] {
        let error = op(
            &conn,
            SftpCmd::Delete {
                id,
                paths: vec![path.into()],
                recursive: true,
            },
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("não é seguro apagar a raiz /"),
            "erro inesperado: {error}"
        );
        assert!(
            dir.path().join("sentinela.txt").exists(),
            "a raiz remota foi alterada por Delete {path}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn chmod_sets_mode() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("arquivo.txt");
    std::fs::write(&file, b"x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    op(
        &conn,
        SftpCmd::Chmod {
            id: 1,
            paths: vec!["/arquivo.txt".into()],
            mode: 0o600,
        },
    )
    .await
    .unwrap();

    let metadata = std::fs::metadata(&file).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

    assert!(conn.send(SftpCmd::List {
        id: 2,
        path: "/".into()
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::Listed { id: 2, result, .. } => {
            let entries = result.unwrap();
            let entry = entries.iter().find(|e| e.name == "arquivo.txt").unwrap();
            assert_eq!(entry.mode, Some(0o600));
        }
        other => panic!("esperava Listed, veio {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_listing_shows_target() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alvo.txt"), b"x").unwrap();
    std::os::unix::fs::symlink("alvo.txt", dir.path().join("link")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::List {
        id: 1,
        path: "/".into()
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::Listed { id: 1, result, .. } => {
            let entries = result.unwrap();
            let link = entries.iter().find(|e| e.name == "link").unwrap();
            assert_eq!(link.kind, EntryKind::Symlink);
            assert_eq!(link.link_target.as_deref(), Some("alvo.txt"));
            let target = entries.iter().find(|e| e.name == "alvo.txt").unwrap();
            assert_eq!(target.kind, EntryKind::File);
            assert_eq!(target.link_target, None);
        }
        other => panic!("esperava Listed, veio {other:?}"),
    }
}

#[tokio::test]
async fn upload_download_file_roundtrip() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    let local_out = tempfile::tempdir().unwrap();
    let bytes = b"conteudo".repeat(1000);
    std::fs::write(local_in.path().join("a.bin"), &bytes).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    start_upload(&conn, 10, vec![local_in.path().join("a.bin")], "/").await;
    assert_eq!(std::fs::read(dir.path().join("a.bin")).unwrap(), bytes);

    start_download(&conn, 11, vec!["/a.bin".into()], local_out.path()).await;
    assert_eq!(
        std::fs::read(local_out.path().join("a.bin")).unwrap(),
        bytes
    );
    assert!(
        !local_out.path().join("a.bin.part").exists(),
        "o download deixou o .part para trás"
    );
}

#[tokio::test]
async fn cancel_running_download_removes_part() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let size = 4 * 1024 * 1024;
    std::fs::write(dir.path().join("slow.bin"), vec![7u8; size]).unwrap();
    let (server, conn) = connect(dir.path()).await;

    // A fixture segura as leituras: o job fica parado no primeiro chunk.
    server.control().pause_reads();
    assert!(conn.send(SftpCmd::Download {
        job_id: 42,
        sources: vec!["/slow.bin".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::JobStarted {
            id,
            items_total,
            bytes_total,
        } => {
            assert_eq!(id, 42);
            assert_eq!(items_total, 1);
            assert_eq!(bytes_total, Some(size as u64));
        }
        other => panic!("esperava JobStarted, veio {other:?}"),
    }

    let part = out.path().join("slow.bin.part");
    wait_until(Duration::from_secs(10), || part.exists()).await;
    assert!(conn.send(SftpCmd::Cancel { job_id: 42 }));
    assert_eq!(wait_done(&conn, 42).await, JobOutcome::Cancelled);
    assert!(!part.exists(), "o .part ficou no destino após cancelar");
    assert!(!out.path().join("slow.bin").exists());
}

#[tokio::test]
async fn queued_cancel_never_starts() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    for index in 1..=4 {
        std::fs::write(
            dir.path().join(format!("slow{index}.bin")),
            vec![index as u8; 512 * 1024],
        )
        .unwrap();
    }
    let (server, conn) = connect(dir.path()).await;

    // A fixture segura as leituras: os três primeiros jobs ocupam os slots e
    // ficam parados no primeiro chunk. O quarto espera na fila.
    server.control().pause_reads();
    for id in 101..=104u64 {
        let index = id - 100;
        assert!(conn.send(SftpCmd::Download {
            job_id: id,
            sources: vec![format!("/slow{index}.bin")],
            dest: out.path().to_path_buf(),
            policy: ConflictPolicy::Overwrite,
        }));
    }
    wait_until(Duration::from_secs(10), || {
        (1..=3).all(|index| out.path().join(format!("slow{index}.bin.part")).exists())
    })
    .await;

    assert!(conn.send(SftpCmd::Cancel { job_id: 104 }));
    let mut events = collect_until_done(&conn, &[104]).await;
    assert!(
        !events.iter().any(|event| matches!(
            event,
            SftpEvent::JobStarted { id: 104, .. } | SftpEvent::JobProgress { id: 104, .. }
        )),
        "job cancelado na fila começou: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(SftpEvent::JobDone {
                id: 104,
                outcome: JobOutcome::Cancelled
            })
        ),
        "esperava JobDone::Cancelled para o job na fila: {events:?}"
    );

    server.control().resume_reads();
    events.extend(collect_until_done(&conn, &[101, 102, 103]).await);

    let done: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SftpEvent::JobDone { id, outcome } => Some((*id, outcome.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        done.len(),
        4,
        "cada job devolve exatamente um JobDone: {done:?}"
    );
    for id in [101u64, 102, 103] {
        assert!(
            done.contains(&(id, JobOutcome::Completed { ignored_links: 0 })),
            "job {id} não concluiu: {done:?}"
        );
    }
    let started: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            SftpEvent::JobStarted { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(
        started.len(),
        3,
        "só os três jobs com slot começam: {started:?}"
    );
    assert!(!started.contains(&104), "o job cancelado na fila começou");

    assert!(!out.path().join("slow4.bin").exists());
    assert!(!out.path().join("slow4.bin.part").exists());
}

#[tokio::test]
async fn drop_mid_transfer_removes_part_and_closes() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("queda.bin"), vec![9u8; 512 * 1024]).unwrap();
    let (server, conn) = connect(dir.path()).await;

    // O primeiro chunk passa; o read seguinte devolve ConnectionLost e a
    // fixture encerra a sessão SSH.
    server.control().kill_after_bytes(CHUNK_SIZE as u64);
    assert!(conn.send(SftpCmd::Download {
        job_id: 77,
        sources: vec!["/queda.bin".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));

    let mut done = false;
    let mut closed = false;
    let mut events = Vec::new();
    while !(done && closed) {
        let event = recv_event(&conn, Duration::from_secs(10)).await;
        match &event {
            SftpEvent::JobStarted { id, .. } | SftpEvent::JobProgress { id, .. } => {
                assert_eq!(*id, 77);
            }
            SftpEvent::JobDone { id, outcome } => {
                assert_eq!(*id, 77);
                assert!(
                    matches!(outcome, JobOutcome::Failed(_)),
                    "esperava Failed, veio {outcome:?}"
                );
                done = true;
            }
            SftpEvent::Closed { .. } => closed = true,
            other => panic!("evento inesperado: {other:?}"),
        }
        events.push(event);
    }

    assert!(
        !out.path().join("queda.bin.part").exists(),
        "o .part ficou no destino após a queda"
    );
    assert!(!out.path().join("queda.bin").exists());
}

#[tokio::test]
async fn disconnect_fails_running_and_queued_jobs() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    for index in 1..=4 {
        std::fs::write(
            dir.path().join(format!("stop{index}.bin")),
            vec![index as u8; 512 * 1024],
        )
        .unwrap();
    }
    let (server, conn) = connect(dir.path()).await;

    server.control().pause_reads();
    for id in 201..=204u64 {
        let index = id - 200;
        assert!(conn.send(SftpCmd::Download {
            job_id: id,
            sources: vec![format!("/stop{index}.bin")],
            dest: out.path().to_path_buf(),
            policy: ConflictPolicy::Overwrite,
        }));
    }
    wait_until(Duration::from_secs(10), || {
        (1..=3).all(|index| out.path().join(format!("stop{index}.bin.part")).exists())
    })
    .await;

    // O Disconnect chega depois dos quatro Downloads; os três primeiros jobs
    // estão parados na fixture, então o quarto está garantidamente na fila.
    assert!(conn.send(SftpCmd::Disconnect));
    let events = collect_until_done(&conn, &[201, 202, 203, 204]).await;
    let done: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SftpEvent::JobDone { id, outcome } => Some((*id, outcome.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        done.len(),
        4,
        "nenhum job pode terminar sem JobDone: {done:?}"
    );
    for (id, outcome) in &done {
        assert!(
            matches!(outcome, JobOutcome::Failed(_)),
            "job {id} devia falhar no encerramento, veio {outcome:?}"
        );
    }
    assert!(
        !events.iter().any(|event| matches!(
            event,
            SftpEvent::JobStarted { id: 204, .. } | SftpEvent::JobProgress { id: 204, .. }
        )),
        "job na fila começou durante o encerramento"
    );
    for index in 1..=4 {
        assert!(
            !out.path().join(format!("stop{index}.bin.part")).exists(),
            "sobrou .part do job {index}"
        );
        assert!(!out.path().join(format!("stop{index}.bin")).exists());
    }
}

#[tokio::test]
async fn paused_download_resumes_after_reads_are_released() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..200_000).map(|index| (index % 251) as u8).collect();
    std::fs::write(dir.path().join("lento.bin"), &bytes).unwrap();
    let (server, conn) = connect(dir.path()).await;

    server.control().pause_reads();
    assert!(conn.send(SftpCmd::Download {
        job_id: 7,
        sources: vec!["/lento.bin".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    let part = out.path().join("lento.bin.part");
    wait_until(Duration::from_secs(10), || part.exists()).await;
    server.control().resume_reads();

    let outcome = wait_job(&conn, 7).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
    assert_eq!(std::fs::read(out.path().join("lento.bin")).unwrap(), bytes);
    assert!(!part.exists());
}

#[tokio::test]
async fn job_emits_final_progress() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let bytes = vec![3u8; 300_000];
    std::fs::write(dir.path().join("final.bin"), &bytes).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 21,
        sources: vec!["/final.bin".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));

    let mut events = Vec::new();
    loop {
        let event = recv_event(&conn, Duration::from_secs(10)).await;
        let done = matches!(event, SftpEvent::JobDone { .. });
        events.push(event);
        if done {
            break;
        }
    }

    let outcome = events
        .iter()
        .find_map(|event| match event {
            SftpEvent::JobDone { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .expect("JobDone ausente");
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });

    let progress: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SftpEvent::JobProgress {
                items_done,
                bytes_done,
                bytes_total,
                current,
                ..
            } => Some((*items_done, *bytes_done, *bytes_total, current.as_str())),
            _ => None,
        })
        .collect();
    let last = progress.last().expect("job não emitiu JobProgress");
    assert_eq!(last.0, 1, "items_done final");
    assert_eq!(last.1, bytes.len() as u64, "bytes_done final");
    assert_eq!(last.2, Some(bytes.len() as u64), "bytes_total final");
    assert_eq!(last.3, "final.bin", "current final");

    // O progresso final é o evento imediatamente anterior ao JobDone.
    let done_index = events
        .iter()
        .position(|event| matches!(event, SftpEvent::JobDone { .. }))
        .expect("JobDone ausente");
    assert!(
        matches!(&events[done_index - 1], SftpEvent::JobProgress { .. }),
        "JobDone veio sem JobProgress final imediatamente antes"
    );
}

#[tokio::test]
async fn download_directory_is_recursive() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("pasta/sub")).unwrap();
    std::fs::write(dir.path().join("pasta/a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("pasta/sub/b.txt"), b"b").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 1,
        sources: vec!["/pasta".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    // Pré-walk: pasta, a.txt, sub, sub/b.txt.
    let (items, bytes, outcome) = wait_job_counts(&conn, 1).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
    assert_eq!(items, 4);
    assert_eq!(bytes, Some(2));
    assert_eq!(std::fs::read(out.path().join("pasta/a.txt")).unwrap(), b"a");
    assert_eq!(
        std::fs::read(out.path().join("pasta/sub/b.txt")).unwrap(),
        b"b"
    );
}

#[tokio::test]
async fn zero_byte_file_roundtrips() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("vazio"), b"").unwrap();
    std::fs::write(local_in.path().join("vazio2"), b"").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 2,
        sources: vec!["/vazio".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    assert_eq!(
        wait_done(&conn, 2).await,
        JobOutcome::Completed { ignored_links: 0 }
    );
    let downloaded = out.path().join("vazio");
    assert!(downloaded.is_file());
    assert_eq!(std::fs::metadata(&downloaded).unwrap().len(), 0);
    assert!(!out.path().join("vazio.part").exists());

    assert!(conn.send(SftpCmd::Upload {
        job_id: 3,
        sources: vec![local_in.path().join("vazio2")],
        dest: "/".into(),
        policy: ConflictPolicy::Overwrite,
    }));
    assert_eq!(
        wait_done(&conn, 3).await,
        JobOutcome::Completed { ignored_links: 0 }
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("vazio2")).unwrap().len(),
        0
    );
}

#[tokio::test]
async fn download_empty_directory_is_created() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("vazia")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 4,
        sources: vec!["/vazia".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    // Pasta vazia conta como 1 diretório e 0 bytes.
    let (items, bytes, outcome) = wait_job_counts(&conn, 4).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
    assert_eq!((items, bytes), (1, Some(0)));
    assert!(out.path().join("vazia").is_dir());
    assert_eq!(
        std::fs::read_dir(out.path().join("vazia")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn upload_directory_is_recursive() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(local_in.path().join("enviada/sub")).unwrap();
    std::fs::write(local_in.path().join("enviada/a.txt"), b"a").unwrap();
    std::fs::write(local_in.path().join("enviada/sub/b.txt"), b"bb").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Upload {
        job_id: 5,
        sources: vec![local_in.path().join("enviada")],
        dest: "/".into(),
        policy: ConflictPolicy::Overwrite,
    }));
    let (items, bytes, outcome) = wait_job_counts(&conn, 5).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
    assert_eq!((items, bytes), (4, Some(3)));
    assert_eq!(
        std::fs::read(dir.path().join("enviada/a.txt")).unwrap(),
        b"a"
    );
    assert_eq!(
        std::fs::read(dir.path().join("enviada/sub/b.txt")).unwrap(),
        b"bb"
    );
}

#[tokio::test]
async fn upload_empty_directory_is_created() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::create_dir(local_in.path().join("vazia")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Upload {
        job_id: 6,
        sources: vec![local_in.path().join("vazia")],
        dest: "/".into(),
        policy: ConflictPolicy::Overwrite,
    }));
    let (items, bytes, outcome) = wait_job_counts(&conn, 6).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
    assert_eq!((items, bytes), (1, Some(0)));
    assert!(dir.path().join("vazia").is_dir());
}

#[tokio::test]
async fn upload_directory_merges_existing_destination() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(local_in.path().join("enviada/sub")).unwrap();
    std::fs::write(local_in.path().join("enviada/a.txt"), b"a").unwrap();
    std::fs::write(local_in.path().join("enviada/sub/b.txt"), b"b").unwrap();
    // Destino remoto já tem a mesma pasta e um arquivo que não pode sumir.
    std::fs::create_dir(dir.path().join("enviada")).unwrap();
    std::fs::write(dir.path().join("enviada/mantido.txt"), b"mantido").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Upload {
        job_id: 20,
        sources: vec![local_in.path().join("enviada")],
        dest: "/".into(),
        policy: ConflictPolicy::Overwrite,
    }));
    let (items, bytes, outcome) = wait_job_counts(&conn, 20).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
    assert_eq!((items, bytes), (4, Some(2)));
    assert_eq!(
        std::fs::read(dir.path().join("enviada/mantido.txt")).unwrap(),
        b"mantido"
    );
    assert_eq!(
        std::fs::read(dir.path().join("enviada/a.txt")).unwrap(),
        b"a"
    );
    assert_eq!(
        std::fs::read(dir.path().join("enviada/sub/b.txt")).unwrap(),
        b"b"
    );
}

#[tokio::test]
async fn download_directory_merges_existing_destination() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("pasta/sub")).unwrap();
    std::fs::write(dir.path().join("pasta/a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("pasta/sub/b.txt"), b"b").unwrap();
    // Destino já tem a mesma pasta e um arquivo que não pode sumir.
    std::fs::create_dir_all(out.path().join("pasta/sub")).unwrap();
    std::fs::write(out.path().join("pasta/mantido.txt"), b"mantido").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 7,
        sources: vec!["/pasta".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    let (items, bytes, outcome) = wait_job_counts(&conn, 7).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
    assert_eq!((items, bytes), (4, Some(2)));
    assert_eq!(
        std::fs::read(out.path().join("pasta/mantido.txt")).unwrap(),
        b"mantido"
    );
    assert_eq!(std::fs::read(out.path().join("pasta/a.txt")).unwrap(), b"a");
    assert_eq!(
        std::fs::read(out.path().join("pasta/sub/b.txt")).unwrap(),
        b"b"
    );
}

#[tokio::test]
async fn download_type_mismatch_fails_without_overwriting() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("pasta")).unwrap();
    std::fs::write(dir.path().join("pasta/a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("arquivo"), b"arquivo remoto").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    // Arquivo local onde a origem é pasta.
    std::fs::write(out.path().join("pasta"), b"arquivo local").unwrap();
    assert!(conn.send(SftpCmd::Download {
        job_id: 8,
        sources: vec!["/pasta".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    match wait_done(&conn, 8).await {
        JobOutcome::Failed(message) => {
            assert!(message.contains("não pasta"), "erro inesperado: {message}")
        }
        other => panic!("esperava Failed, veio {other:?}"),
    }
    assert_eq!(
        std::fs::read(out.path().join("pasta")).unwrap(),
        b"arquivo local"
    );

    // Pasta local onde a origem é arquivo.
    std::fs::create_dir(out.path().join("arquivo")).unwrap();
    assert!(conn.send(SftpCmd::Download {
        job_id: 9,
        sources: vec!["/arquivo".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    match wait_done(&conn, 9).await {
        JobOutcome::Failed(message) => {
            assert!(
                message.contains("não arquivo"),
                "erro inesperado: {message}"
            )
        }
        other => panic!("esperava Failed, veio {other:?}"),
    }
    assert!(out.path().join("arquivo").is_dir());
    assert!(!out.path().join("arquivo.part").exists());
}

#[tokio::test]
async fn upload_type_mismatch_fails_without_overwriting() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(local_in.path().join("bloco")).unwrap();
    std::fs::write(local_in.path().join("bloco/a.txt"), b"a").unwrap();
    std::fs::write(local_in.path().join("arquivo"), b"arquivo local").unwrap();
    std::fs::write(dir.path().join("bloco"), b"arquivo remoto").unwrap();
    std::fs::create_dir(dir.path().join("arquivo")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    // Arquivo remoto onde a origem é pasta.
    assert!(conn.send(SftpCmd::Upload {
        job_id: 10,
        sources: vec![local_in.path().join("bloco")],
        dest: "/".into(),
        policy: ConflictPolicy::Overwrite,
    }));
    match wait_done(&conn, 10).await {
        JobOutcome::Failed(message) => {
            assert!(message.contains("não pasta"), "erro inesperado: {message}")
        }
        other => panic!("esperava Failed, veio {other:?}"),
    }
    assert_eq!(
        std::fs::read(dir.path().join("bloco")).unwrap(),
        b"arquivo remoto"
    );

    // Pasta remota onde a origem é arquivo.
    assert!(conn.send(SftpCmd::Upload {
        job_id: 11,
        sources: vec![local_in.path().join("arquivo")],
        dest: "/".into(),
        policy: ConflictPolicy::Overwrite,
    }));
    match wait_done(&conn, 11).await {
        JobOutcome::Failed(message) => {
            assert!(
                message.contains("não arquivo"),
                "erro inesperado: {message}"
            )
        }
        other => panic!("esperava Failed, veio {other:?}"),
    }
    assert!(dir.path().join("arquivo").is_dir());
}

#[tokio::test]
async fn download_root_source_is_refused() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let (_server, conn) = connect(dir.path()).await;

    for (job_id, source) in [(12u64, "/"), (13u64, "/..")] {
        assert!(conn.send(SftpCmd::Download {
            job_id,
            sources: vec![source.into()],
            dest: out.path().to_path_buf(),
            policy: ConflictPolicy::Overwrite,
        }));
        assert_eq!(
            wait_done(&conn, job_id).await,
            JobOutcome::Failed("selecione uma pasta".into())
        );
    }
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn recursive_upload_skips_symlink_loop() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::create_dir(local_in.path().join("sub")).unwrap();
    std::fs::write(local_in.path().join("a.txt"), b"a").unwrap();
    std::fs::write(local_in.path().join("sub/b.txt"), b"b").unwrap();
    // Link para a própria pasta: seguir entraria em loop.
    std::os::unix::fs::symlink(local_in.path(), local_in.path().join("loop")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Upload {
        job_id: 14,
        sources: vec![local_in.path().to_path_buf()],
        dest: "/".into(),
        policy: ConflictPolicy::Overwrite,
    }));
    let (items, bytes, outcome) = wait_job_counts(&conn, 14).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 1 });
    assert_eq!((items, bytes), (4, Some(2)));

    let remote = dir.path().join(local_in.path().file_name().unwrap());
    assert_eq!(std::fs::read(remote.join("a.txt")).unwrap(), b"a");
    assert_eq!(std::fs::read(remote.join("sub/b.txt")).unwrap(), b"b");
    assert!(!remote.join("loop").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn recursive_download_skips_symlink_loop() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("pasta/sub")).unwrap();
    std::fs::write(dir.path().join("pasta/a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("pasta/sub/b.txt"), b"b").unwrap();
    std::os::unix::fs::symlink("..", dir.path().join("pasta/loop")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 15,
        sources: vec!["/pasta".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    let (items, bytes, outcome) = wait_job_counts(&conn, 15).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 1 });
    assert_eq!((items, bytes), (4, Some(2)));
    assert_eq!(std::fs::read(out.path().join("pasta/a.txt")).unwrap(), b"a");
    assert_eq!(
        std::fs::read(out.path().join("pasta/sub/b.txt")).unwrap(),
        b"b"
    );
    assert!(!out.path().join("pasta/loop").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn selected_symlink_to_file_is_followed() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alvo.txt"), b"alvo").unwrap();
    std::os::unix::fs::symlink("alvo.txt", dir.path().join("link")).unwrap();
    std::fs::write(local_in.path().join("alvo2.txt"), b"alvo2").unwrap();
    std::os::unix::fs::symlink("alvo2.txt", local_in.path().join("link2")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    start_download(&conn, 16, vec!["/link".into()], out.path()).await;
    assert_eq!(std::fs::read(out.path().join("link")).unwrap(), b"alvo");

    start_upload(&conn, 17, vec![local_in.path().join("link2")], "/").await;
    assert_eq!(std::fs::read(dir.path().join("link2")).unwrap(), b"alvo2");
}

#[cfg(unix)]
#[tokio::test]
async fn selected_symlink_to_directory_is_refused() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("pasta")).unwrap();
    std::os::unix::fs::symlink("pasta", dir.path().join("link_dir")).unwrap();
    std::fs::create_dir(local_in.path().join("pasta")).unwrap();
    std::os::unix::fs::symlink("pasta", local_in.path().join("link_dir")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 18,
        sources: vec!["/link_dir".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    match wait_done(&conn, 18).await {
        JobOutcome::Failed(message) => assert!(
            message.contains("link simbólico para pasta"),
            "erro inesperado: {message}"
        ),
        other => panic!("esperava Failed, veio {other:?}"),
    }
    assert!(!out.path().join("link_dir").exists());

    assert!(conn.send(SftpCmd::Upload {
        job_id: 19,
        sources: vec![local_in.path().join("link_dir")],
        dest: "/".into(),
        policy: ConflictPolicy::Overwrite,
    }));
    match wait_done(&conn, 19).await {
        JobOutcome::Failed(message) => assert!(
            message.contains("link simbólico para pasta"),
            "erro inesperado: {message}"
        ),
        other => panic!("esperava Failed, veio {other:?}"),
    }
    // O link remoto que já existia não foi substituído pelo upload.
    assert!(std::fs::symlink_metadata(dir.path().join("link_dir"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn local_dest_rejects_traversal_and_nul() {
    let root = Path::new("/tmp/dest");
    assert!(local_dest(root, "../fora").is_err());
    assert!(local_dest(root, "a/../../fora").is_err());
    assert!(local_dest(root, "a\0b").is_err());
    assert!(local_dest(root, "/abs").is_err());
    assert_eq!(local_dest(root, "a/b.txt").unwrap(), root.join("a/b.txt"));

    // Componente vazio, "." e ".." nunca passam.
    assert!(local_dest(root, "").is_err());
    assert!(local_dest(root, "..").is_err());
    assert!(local_dest(root, ".").is_err());
    assert!(local_dest(root, "a//b").is_err());
    assert!(local_dest(root, "a/./b").is_err());
    assert!(local_dest(root, "a/").is_err());
    assert!(local_dest(root, "/").is_err());
}

#[cfg(unix)]
#[test]
fn local_dest_keeps_unix_backslash_name() {
    let root = Path::new("/tmp/dest");
    // Em Unix a barra invertida é um caractere válido de nome.
    assert_eq!(local_dest(root, "a\\b.txt").unwrap(), root.join("a\\b.txt"));
}

#[cfg(windows)]
#[test]
fn local_dest_rejects_windows_escapes() {
    let root = Path::new(r"C:\dest");
    // Separador "\", prefixo de unidade e raiz não podem escapar do destino.
    assert!(local_dest(root, r"a\b").is_err());
    assert!(local_dest(root, r"\abs").is_err());
    assert!(local_dest(root, r"C:\abs").is_err());
    assert!(local_dest(root, r"C:rel").is_err());
    assert!(local_dest(root, r"\\server\share").is_err());
    assert_eq!(local_dest(root, "a/b.txt").unwrap(), root.join("a/b.txt"));
}

#[test]
fn normalize_remote_collapses_and_clamps() {
    assert_eq!(normalize_remote("/a//b/../c/").as_deref(), Some("/a/c"));
    assert_eq!(normalize_remote("/..").as_deref(), Some("/"));
    assert_eq!(normalize_remote("relativo"), None);
    assert_eq!(normalize_remote(""), None);
}

/// Consome eventos até o primeiro Conflict do job, devolvendo os campos do
/// evento. JobStarted/JobProgress são consumidos normalmente no caminho.
async fn wait_conflict(
    conn: &SftpConnection,
    job_id: u64,
) -> (Direction, String, String, u64, u64) {
    loop {
        match recv_event(conn, Duration::from_secs(10)).await {
            SftpEvent::Conflict {
                id,
                direction,
                source,
                dest,
                source_size,
                dest_size,
            } => {
                assert_eq!(id, job_id, "Conflict com id diferente");
                return (direction, source, dest, source_size, dest_size);
            }
            SftpEvent::JobStarted { id, .. } | SftpEvent::JobProgress { id, .. } => {
                assert_eq!(id, job_id);
            }
            other => panic!("esperava Conflict do job {job_id}, veio {other:?}"),
        }
    }
}

#[tokio::test]
async fn download_conflict_rename_creates_numbered_file() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"remoto").unwrap();
    std::fs::write(out.path().join("a.txt"), b"local").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 30,
        sources: vec!["/a.txt".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Ask,
    }));

    let (direction, source, dest, source_size, dest_size) = wait_conflict(&conn, 30).await;
    assert_eq!(direction, Direction::Download);
    assert_eq!(source, "/a.txt");
    assert_eq!(dest, out.path().join("a.txt").display().to_string());
    assert_eq!(source_size, 6);
    assert_eq!(dest_size, 5);

    assert!(conn.send(SftpCmd::ResolveConflict {
        job_id: 30,
        decision: ConflictDecision::Rename,
        apply_all: false,
    }));
    assert_eq!(
        wait_done(&conn, 30).await,
        JobOutcome::Completed { ignored_links: 0 }
    );

    // O destino original fica intacto e o conteúdo remoto vai para o nome
    // numerado, preservando a extensão.
    assert_eq!(std::fs::read(out.path().join("a.txt")).unwrap(), b"local");
    assert_eq!(
        std::fs::read(out.path().join("a (2).txt")).unwrap(),
        b"remoto"
    );
    assert!(!out.path().join("a.txt.part").exists());
}

#[tokio::test]
async fn apply_all_resolves_remaining_conflicts() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(dir.path().join(name), b"remoto").unwrap();
        std::fs::write(out.path().join(name), b"local").unwrap();
    }
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 31,
        sources: vec!["/a.txt".into(), "/b.txt".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Ask,
    }));

    let (_, source, ..) = wait_conflict(&conn, 31).await;
    assert_eq!(source, "/a.txt", "o primeiro conflito é o primeiro item");
    assert!(conn.send(SftpCmd::ResolveConflict {
        job_id: 31,
        decision: ConflictDecision::Overwrite,
        apply_all: true,
    }));

    let events = collect_until_done(&conn, &[31]).await;
    let conflicts = events
        .iter()
        .filter(|event| matches!(event, SftpEvent::Conflict { .. }))
        .count();
    assert_eq!(conflicts, 0, "apply_all não pode pedir decisão de novo");
    assert!(
        matches!(
            events.last(),
            Some(SftpEvent::JobDone {
                id: 31,
                outcome: JobOutcome::Completed { ignored_links: 0 }
            })
        ),
        "esperava JobDone::Completed: {events:?}"
    );
    assert_eq!(std::fs::read(out.path().join("a.txt")).unwrap(), b"remoto");
    assert_eq!(std::fs::read(out.path().join("b.txt")).unwrap(), b"remoto");
}

#[tokio::test]
async fn download_conflict_skip_leaves_destination() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"remoto").unwrap();
    std::fs::write(out.path().join("a.txt"), b"local").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 32,
        sources: vec!["/a.txt".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Ask,
    }));
    wait_conflict(&conn, 32).await;
    assert!(conn.send(SftpCmd::ResolveConflict {
        job_id: 32,
        decision: ConflictDecision::Skip,
        apply_all: false,
    }));
    assert_eq!(
        wait_done(&conn, 32).await,
        JobOutcome::Completed { ignored_links: 0 }
    );

    assert_eq!(
        std::fs::read(out.path().join("a.txt")).unwrap(),
        b"local",
        "Skip não pode tocar no destino"
    );
    assert!(!out.path().join("a (2).txt").exists());
    assert!(!out.path().join("a.txt.part").exists());
}

#[tokio::test]
async fn conflict_policy_skip_does_not_ask() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"remoto").unwrap();
    std::fs::write(out.path().join("a.txt"), b"local").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 33,
        sources: vec!["/a.txt".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Skip,
    }));
    let events = collect_until_done(&conn, &[33]).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SftpEvent::Conflict { .. })),
        "policy Skip não pede decisão: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(SftpEvent::JobDone {
                id: 33,
                outcome: JobOutcome::Completed { ignored_links: 0 }
            })
        ),
        "esperava JobDone::Completed: {events:?}"
    );
    assert_eq!(
        std::fs::read(out.path().join("a.txt")).unwrap(),
        b"local",
        "policy Skip não pode tocar no destino"
    );
}

#[tokio::test]
async fn conflict_policy_ask_merges_directories_without_prompt() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("pasta")).unwrap();
    std::fs::write(dir.path().join("pasta/a.txt"), b"a").unwrap();
    std::fs::create_dir(out.path().join("pasta")).unwrap();
    std::fs::write(out.path().join("pasta/mantido.txt"), b"mantido").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 36,
        sources: vec!["/pasta".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Ask,
    }));
    let events = collect_until_done(&conn, &[36]).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SftpEvent::Conflict { .. })),
        "pastas mesclam sem perguntar: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(SftpEvent::JobDone {
                id: 36,
                outcome: JobOutcome::Completed { ignored_links: 0 }
            })
        ),
        "esperava JobDone::Completed: {events:?}"
    );
    assert_eq!(std::fs::read(out.path().join("pasta/a.txt")).unwrap(), b"a");
    assert_eq!(
        std::fs::read(out.path().join("pasta/mantido.txt")).unwrap(),
        b"mantido"
    );
}

#[tokio::test]
async fn upload_conflict_rename_creates_numbered_file() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"remoto").unwrap();
    std::fs::write(local_in.path().join("a.txt"), b"local").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Upload {
        job_id: 34,
        sources: vec![local_in.path().join("a.txt")],
        dest: "/".into(),
        policy: ConflictPolicy::Ask,
    }));

    let (direction, source, dest, source_size, dest_size) = wait_conflict(&conn, 34).await;
    assert_eq!(direction, Direction::Upload);
    assert_eq!(source, local_in.path().join("a.txt").display().to_string());
    assert_eq!(dest, "/a.txt");
    assert_eq!(source_size, 5);
    assert_eq!(dest_size, 6);

    assert!(conn.send(SftpCmd::ResolveConflict {
        job_id: 34,
        decision: ConflictDecision::Rename,
        apply_all: false,
    }));
    assert_eq!(
        wait_done(&conn, 34).await,
        JobOutcome::Completed { ignored_links: 0 }
    );

    assert_eq!(
        std::fs::read(dir.path().join("a.txt")).unwrap(),
        b"remoto",
        "o destino remoto original fica intacto"
    );
    assert_eq!(
        std::fs::read(dir.path().join("a (2).txt")).unwrap(),
        b"local",
        "o upload renomeado preserva a extensão"
    );
}

#[tokio::test]
async fn cancel_while_waiting_conflict_ends_cancelled() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"remoto").unwrap();
    std::fs::write(out.path().join("a.txt"), b"local").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 35,
        sources: vec!["/a.txt".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Ask,
    }));
    wait_conflict(&conn, 35).await;
    assert!(conn.send(SftpCmd::Cancel { job_id: 35 }));
    assert_eq!(wait_done(&conn, 35).await, JobOutcome::Cancelled);

    assert_eq!(std::fs::read(out.path().join("a.txt")).unwrap(), b"local");
    assert!(!out.path().join("a (2).txt").exists());
    assert!(!out.path().join("a.txt.part").exists());
}

/// A Task 5 recusava qualquer link no destino do upload. try_exists segue
/// links: um link quebrado parece ausente e o upload escreveria no alvo.
#[cfg(unix)]
#[tokio::test]
async fn upload_to_dangling_symlink_fails_and_keeps_link() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::write(local_in.path().join("a.txt"), b"novo").unwrap();
    let target = dir.path().join("alvo-inexistente");
    let link = dir.path().join("a.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Upload {
        job_id: 40,
        sources: vec![local_in.path().join("a.txt")],
        dest: "/".into(),
        policy: ConflictPolicy::Ask,
    }));
    match wait_done(&conn, 40).await {
        JobOutcome::Failed(message) => {
            assert!(
                message.contains("link simbólico"),
                "erro inesperado: {message}"
            );
        }
        other => panic!("esperava Failed, veio {other:?}"),
    }
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "o link de destino não pode ser removido nem substituído"
    );
    assert!(
        !target.exists(),
        "o upload não pode escrever através do link"
    );
}

/// O Rename procura o primeiro nome livre sem seguir link: "a (2).txt"
/// ocupado por um link quebrado não pode ser escolhido, senão o upload
/// escreveria no alvo do link.
#[cfg(unix)]
#[tokio::test]
async fn upload_conflict_rename_skips_symlink_candidate() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"remoto").unwrap();
    std::fs::write(local_in.path().join("a.txt"), b"local").unwrap();
    let target = dir.path().join("alvo-inexistente");
    std::os::unix::fs::symlink(&target, dir.path().join("a (2).txt")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Upload {
        job_id: 41,
        sources: vec![local_in.path().join("a.txt")],
        dest: "/".into(),
        policy: ConflictPolicy::Ask,
    }));
    wait_conflict(&conn, 41).await;
    assert!(conn.send(SftpCmd::ResolveConflict {
        job_id: 41,
        decision: ConflictDecision::Rename,
        apply_all: false,
    }));
    assert_eq!(
        wait_done(&conn, 41).await,
        JobOutcome::Completed { ignored_links: 0 }
    );

    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"remoto");
    assert!(
        std::fs::symlink_metadata(dir.path().join("a (2).txt"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "o candidato ocupado por link não pode ser substituído"
    );
    assert!(
        !target.exists(),
        "o upload não pode escrever através do link"
    );
    assert_eq!(
        std::fs::read(dir.path().join("a (3).txt")).unwrap(),
        b"local",
        "deve pular o nome ocupado e usar o próximo livre"
    );
}

async fn connect(dir: &Path) -> (TestSftpServer, SftpConnection) {
    let server = TestSftpServer::start(dir).await;
    std::env::set_var("SSHC_KNOWN_HOSTS", dir.join("known_hosts"));
    let session = test_session(server.port());
    let conn = tokio::task::spawn_blocking(move || {
        sshc_net::sftp::connect(&session, Credentials::Password("x".into()))
    })
    .await
    .unwrap()
    .unwrap();
    (server, conn)
}

async fn op(conn: &SftpConnection, command: SftpCmd) -> Result<(), String> {
    let id = match &command {
        SftpCmd::Mkdir { id, .. }
        | SftpCmd::Rename { id, .. }
        | SftpCmd::Delete { id, .. }
        | SftpCmd::Chmod { id, .. } => *id,
        other => panic!("comando sem OpDone: {other:?}"),
    };
    assert!(conn.send(command), "conexão SFTP encerrada");
    match recv_event(conn, Duration::from_secs(10)).await {
        SftpEvent::OpDone { id: done, result } => {
            assert_eq!(done, id, "OpDone com id diferente do comando");
            result
        }
        other => panic!("esperava OpDone, veio {other:?}"),
    }
}

async fn start_upload(conn: &SftpConnection, job_id: u64, sources: Vec<PathBuf>, dest: &str) {
    assert!(conn.send(SftpCmd::Upload {
        job_id,
        sources,
        dest: dest.to_string(),
        policy: ConflictPolicy::Overwrite,
    }));
    let outcome = wait_job(conn, job_id).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
}

async fn start_download(conn: &SftpConnection, job_id: u64, sources: Vec<String>, dest: &Path) {
    assert!(conn.send(SftpCmd::Download {
        job_id,
        sources,
        dest: dest.to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    let outcome = wait_job(conn, job_id).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
}

/// Consome eventos até JobDone e exige que JobStarted tenha vindo antes.
async fn wait_job(conn: &SftpConnection, job_id: u64) -> JobOutcome {
    let (items, _, outcome) = wait_job_counts(conn, job_id).await;
    assert_eq!(items, 1, "job de arquivo único tem 1 item");
    outcome
}

/// Consome eventos até JobDone, devolvendo os totais anunciados no JobStarted.
/// Para jobs concluídos confere que o progresso final bate com os totais.
async fn wait_job_counts(conn: &SftpConnection, job_id: u64) -> (u64, Option<u64>, JobOutcome) {
    let mut started: Option<(u64, Option<u64>)> = None;
    let mut last_progress: Option<(u64, u64, Option<u64>)> = None;
    loop {
        match recv_event(conn, Duration::from_secs(10)).await {
            SftpEvent::JobStarted {
                id,
                items_total,
                bytes_total,
            } => {
                assert_eq!(id, job_id, "JobStarted com id diferente");
                assert!(
                    started.replace((items_total, bytes_total)).is_none(),
                    "JobStarted duplicado"
                );
            }
            SftpEvent::JobProgress {
                id,
                items_done,
                bytes_done,
                bytes_total,
                ..
            } => {
                assert_eq!(id, job_id);
                last_progress = Some((items_done, bytes_done, bytes_total));
            }
            SftpEvent::JobDone { id, outcome } => {
                assert_eq!(id, job_id, "JobDone com id diferente");
                let (items_total, bytes_total) = started.expect("JobDone sem JobStarted");
                if matches!(outcome, JobOutcome::Completed { .. }) {
                    let last = last_progress.expect("job concluído sem JobProgress final");
                    assert_eq!(last.0, items_total, "items_done final");
                    assert_eq!(last.1, bytes_total.unwrap_or(0), "bytes_done final");
                    assert_eq!(last.2, bytes_total, "bytes_total final");
                }
                return (items_total, bytes_total, outcome);
            }
            other => panic!("esperava eventos do job {job_id}, veio {other:?}"),
        }
    }
}

/// Consome eventos até JobDone, sem exigir JobStarted (jobs que falham no
/// planejamento não iniciam).
async fn wait_done(conn: &SftpConnection, job_id: u64) -> JobOutcome {
    loop {
        match recv_event(conn, Duration::from_secs(10)).await {
            SftpEvent::JobStarted { id, .. } | SftpEvent::JobProgress { id, .. } => {
                assert_eq!(id, job_id);
            }
            SftpEvent::JobDone { id, outcome } => {
                assert_eq!(id, job_id, "JobDone com id diferente");
                return outcome;
            }
            other => panic!("esperava eventos do job {job_id}, veio {other:?}"),
        }
    }
}

/// Consome eventos até ver JobDone de todos os ids pedidos, devolvendo tudo
/// o que chegou no caminho (inclusive JobStarted/JobProgress/Closed).
async fn collect_until_done(conn: &SftpConnection, ids: &[u64]) -> Vec<SftpEvent> {
    let mut events = Vec::new();
    let mut done: Vec<u64> = Vec::new();
    while done.len() < ids.len() {
        let event = recv_event(conn, Duration::from_secs(10)).await;
        if let SftpEvent::JobDone { id, .. } = &event {
            assert!(ids.contains(id), "JobDone inesperado do job {id}");
            assert!(!done.contains(id), "JobDone duplicado do job {id}");
            done.push(*id);
        }
        events.push(event);
    }
    events
}

async fn wait_until(timeout: Duration, condition: impl Fn() -> bool) {
    let start = std::time::Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < timeout,
            "tempo esgotado esperando condição"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn test_session(port: u16) -> sshc_core::Session {
    sshc_core::Session {
        name: "sftp".into(),
        protocol: Protocol::Ssh,
        host: "127.0.0.1".into(),
        port,
        user: Some("teste".into()),
        emulation: "xterm-256color".into(),
        auth: None,
        description: None,
        baud: None,
    }
}

async fn recv_event(conn: &SftpConnection, timeout: Duration) -> SftpEvent {
    let start = std::time::Instant::now();
    loop {
        match conn.events.try_recv() {
            Ok(event) => return event,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                assert!(
                    start.elapsed() < timeout,
                    "tempo esgotado esperando evento SFTP"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("conexão SFTP encerrou sem evento")
            }
        }
    }
}

struct TestSftpServer {
    port: u16,
    task: tokio::task::JoinHandle<()>,
    control: TestControl,
}

impl TestSftpServer {
    async fn start(root: &Path) -> Self {
        let host_key = russh::keys::PrivateKey::from_openssh(TEST_KEY).unwrap();
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            inactivity_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let control = TestControl::default();
        let mut server = FixtureServer {
            root: root.to_path_buf(),
            channels: Arc::new(Mutex::new(HashMap::new())),
            control: control.clone(),
        };
        let task = tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });
        tokio::time::sleep(Duration::from_millis(150)).await;

        Self {
            port,
            task,
            control,
        }
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn control(&self) -> &TestControl {
        &self.control
    }
}

/// Controle de pausa compartilhado entre teste e fixture. Sem ele o
/// cancelamento de um download em andamento não seria determinístico.
#[derive(Clone, Default)]
struct TestControl {
    paused: Arc<AtomicBool>,
    reads_resumed: Arc<Notify>,
    /// Depois de N bytes lidos, a fixture derruba a sessão SSH.
    kill_after: Arc<Mutex<Option<u64>>>,
    bytes_read: Arc<AtomicU64>,
}

impl TestControl {
    fn pause_reads(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// Arma a queda de conexão: depois de limit bytes servidos, o próximo
    /// read devolve ConnectionLost e encerra a sessão SSH.
    fn kill_after_bytes(&self, limit: u64) {
        *self.kill_after.lock().unwrap() = Some(limit);
        self.bytes_read.store(0, Ordering::SeqCst);
    }

    fn kill_due(&self) -> bool {
        match *self.kill_after.lock().unwrap() {
            Some(limit) => self.bytes_read.load(Ordering::SeqCst) >= limit,
            None => false,
        }
    }

    fn add_bytes_read(&self, bytes: u64) {
        self.bytes_read.fetch_add(bytes, Ordering::SeqCst);
    }

    fn resume_reads(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.reads_resumed.notify_waiters();
    }

    /// Bloqueia enquanto a pausa estiver ativa. O enable registra o waiter
    /// antes da última checagem, então um resume concorrente não é perdido.
    async fn wait_reads(&self) {
        loop {
            if !self.paused.load(Ordering::SeqCst) {
                return;
            }
            let notified = self.reads_resumed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.paused.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for TestSftpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Channels = Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>;

#[derive(Clone)]
struct FixtureServer {
    root: PathBuf,
    channels: Channels,
    control: TestControl,
}

impl server::Server for FixtureServer {
    type Handler = FixtureSession;

    fn new_client(&mut self, _addr: Option<std::net::SocketAddr>) -> FixtureSession {
        FixtureSession {
            root: self.root.clone(),
            channels: self.channels.clone(),
            control: self.control.clone(),
        }
    }
}

struct FixtureSession {
    root: PathBuf,
    channels: Channels,
    control: TestControl,
}

impl server::Handler for FixtureSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.channels.lock().unwrap().insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        let channel = self.channels.lock().unwrap().remove(&channel_id);
        let Some(channel) = channel else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        session.channel_success(channel_id)?;
        russh_sftp::server::run(
            channel.into_stream(),
            SftpFs::new(self.root.clone(), self.control.clone(), session.handle()),
        )
        .await;
        Ok(())
    }
}

struct SftpFs {
    root: PathBuf,
    control: TestControl,
    /// Handle da sessão SSH, usado pelo kill_after_bytes para simular queda.
    handle: SessionHandle,
    dirs: HashMap<String, VecDeque<File>>,
    files: HashMap<String, std::fs::File>,
}

impl SftpFs {
    fn new(root: PathBuf, control: TestControl, handle: SessionHandle) -> Self {
        Self {
            root,
            control,
            handle,
            dirs: HashMap::new(),
            files: HashMap::new(),
        }
    }

    /// Mapeia um caminho remoto absoluto para o tempdir do servidor. Recusa
    /// travessia (..), NUL, caminhos relativos e componentes que escapem da raiz.
    fn local(&self, path: &str) -> Result<PathBuf, StatusCode> {
        if path.is_empty() || path.contains('\0') {
            return Err(StatusCode::NoSuchFile);
        }
        if path.split('/').any(|part| part == "..") {
            return Err(StatusCode::NoSuchFile);
        }
        let normalized = normalize_remote(path).ok_or(StatusCode::NoSuchFile)?;
        let mut local = self.root.clone();
        for part in normalized.trim_start_matches('/').split('/') {
            if part.is_empty() {
                continue;
            }
            let component = Path::new(part);
            if component.is_absolute() || component.components().count() != 1 {
                return Err(StatusCode::NoSuchFile);
            }
            local.push(part);
            if !local.starts_with(&self.root) {
                return Err(StatusCode::NoSuchFile);
            }
        }
        Ok(local)
    }
}

fn status_code(error: &std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en-US".to_string(),
    }
}

fn attributes(metadata: &std::fs::Metadata) -> FileAttributes {
    let mut attrs = FileAttributes::from(metadata);
    if metadata.file_type().is_symlink() {
        let permissions = attrs.permissions.unwrap_or(0) & 0o7777;
        attrs.permissions = Some(FileMode::LNK.bits() | permissions);
    }
    attrs
}

impl russh_sftp::server::Handler for SftpFs {
    type Error = StatusCode;

    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, StatusCode> {
        let local = self.local(&filename)?;
        let file = std::fs::OpenOptions::from(pflags)
            .open(&local)
            .map_err(|error| status_code(&error))?;
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|error| status_code(&error))?;
        }
        #[cfg(not(unix))]
        let _ = attrs;
        let handle = format!("file-{id}");
        self.files.insert(handle.clone(), file);
        Ok(Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, StatusCode> {
        // Pausa antes de qualquer byte: é o que torna o cancelamento
        // determinístico no teste.
        let control = self.control.clone();
        control.wait_reads().await;
        if control.kill_due() {
            // A queda derruba a sessão inteira, não só esta requisição: o
            // cliente precisa ver JobDone::Failed e depois o evento Closed.
            let handle = self.handle.clone();
            let _ = handle
                .disconnect(
                    Disconnect::ConnectionLost,
                    "queda simulada no meio da transferência".to_string(),
                    "en".to_string(),
                )
                .await;
            return Err(StatusCode::ConnectionLost);
        }

        let Some(file) = self.files.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| status_code(&error))?;
        // Limita o pacote de resposta (o cliente pode pedir 256 KiB).
        let mut data = vec![0u8; (len as usize).min(64 * 1024)];
        let read = file.read(&mut data).map_err(|error| status_code(&error))?;
        if read == 0 {
            return Err(StatusCode::Eof);
        }
        control.add_bytes_read(read as u64);
        data.truncate(read);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        let Some(file) = self.files.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| status_code(&error))?;
        file.write_all(&data).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        let Some(file) = self.files.get(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        let metadata = file.metadata().map_err(|error| status_code(&error))?;
        Ok(Attrs {
            id,
            attrs: attributes(&metadata),
        })
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, StatusCode> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, _path: String) -> Result<Name, StatusCode> {
        Ok(Name {
            id,
            files: vec![File::dummy("/")],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusCode> {
        let local = self.local(&path)?;
        let metadata = std::fs::metadata(&local).map_err(|error| status_code(&error))?;
        if !metadata.is_dir() {
            return Err(StatusCode::Failure);
        }

        let mut files = Vec::new();
        let entries = std::fs::read_dir(&local).map_err(|error| status_code(&error))?;
        for entry in entries {
            let entry = entry.map_err(|error| status_code(&error))?;
            let metadata =
                std::fs::symlink_metadata(entry.path()).map_err(|error| status_code(&error))?;
            files.push(File::new(
                entry.file_name().to_string_lossy().into_owned(),
                attributes(&metadata),
            ));
        }
        files.sort_by(|left, right| left.filename.cmp(&right.filename));

        let handle = format!("dir-{id}");
        self.dirs.insert(handle.clone(), files.into());
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusCode> {
        let Some(files) = self.dirs.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        if files.is_empty() {
            return Err(StatusCode::Eof);
        }
        let files = files.drain(..).collect();
        Ok(Name { id, files })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let local = self.local(&path)?;
        let metadata = std::fs::symlink_metadata(&local).map_err(|error| status_code(&error))?;
        Ok(Attrs {
            id,
            attrs: attributes(&metadata),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let local = self.local(&path)?;
        let metadata = std::fs::metadata(&local).map_err(|error| status_code(&error))?;
        Ok(Attrs {
            id,
            attrs: attributes(&metadata),
        })
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, StatusCode> {
        let local = self.local(&path)?;
        std::fs::create_dir(&local).map_err(|error| status_code(&error))?;
        #[cfg(unix)]
        if let Some(mode) = _attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|error| status_code(&error))?;
        }
        Ok(ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
        let local = self.local(&path)?;
        std::fs::remove_dir(&local).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, StatusCode> {
        let local = self.local(&filename)?;
        std::fs::remove_file(&local).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, StatusCode> {
        let from = self.local(&oldpath)?;
        let to = self.local(&newpath)?;
        std::fs::rename(&from, &to).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, StatusCode> {
        let local = self.local(&path)?;
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|error| status_code(&error))?;
        }
        #[cfg(not(unix))]
        if attrs.permissions.is_some() {
            return Err(StatusCode::OpUnsupported);
        }
        Ok(ok_status(id))
    }

    #[cfg(unix)]
    async fn symlink(
        &mut self,
        id: u32,
        linkpath: String,
        targetpath: String,
    ) -> Result<Status, StatusCode> {
        let link = self.local(&linkpath)?;
        std::os::unix::fs::symlink(&targetpath, &link).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    #[cfg(not(unix))]
    async fn symlink(
        &mut self,
        _id: u32,
        _linkpath: String,
        _targetpath: String,
    ) -> Result<Status, StatusCode> {
        Err(StatusCode::OpUnsupported)
    }

    #[cfg(unix)]
    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, StatusCode> {
        let local = self.local(&path)?;
        let target = std::fs::read_link(&local).map_err(|error| status_code(&error))?;
        Ok(Name {
            id,
            files: vec![File::new(
                target.to_string_lossy().into_owned(),
                FileAttributes::dummy(),
            )],
        })
    }

    #[cfg(not(unix))]
    async fn readlink(&mut self, _id: u32, _path: String) -> Result<Name, StatusCode> {
        Err(StatusCode::OpUnsupported)
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
        self.dirs.remove(&handle);
        self.files.remove(&handle);
        Ok(ok_status(id))
    }
}
