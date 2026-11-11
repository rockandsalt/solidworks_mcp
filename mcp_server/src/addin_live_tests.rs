//! Opt-in integration checks against a running patched add-in. No CAD edits.
//! Run: cargo test expired_queued_macro_never_launches -- --ignored --nocapture
//! Run on a detailed model: cargo test expired_mesh_releases_model_lock -- --ignored --nocapture
use super::*;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn vba_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    assert!(
        text.is_ascii(),
        "this .swb fixture needs an ASCII temporary path"
    );
    format!("\"{}\"", text.replace('"', "\"\""))
}

#[tokio::test]
#[ignore = "requires SolidWorks with the patched add-in; holds the model lock for four seconds"]
async fn expired_queued_macro_never_launches() {
    let folder = std::env::temp_dir().join(format!(
        "sld_mcp-live-cancel-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir(&folder).unwrap();
    eprintln!("Live cancellation evidence: {}", folder.display());
    let entered = folder.join("holder-started.txt");
    let finished = folder.join("holder-finished.txt");
    let launched = folder.join("queued-launched.txt");
    let holder_file = folder.join("holder.swb");
    let queued_file = folder.join("queued.swb");
    let barrier_file = folder.join("barrier.swb");
    std::fs::write(&barrier_file, "Sub main()\r\nEnd Sub\r\n").unwrap();
    std::fs::write(
        &holder_file,
        format!(
            "Sub main()\r\n\
         Dim files As Object\r\n\
         Set files = CreateObject(\"Scripting.FileSystemObject\")\r\n\
         files.CreateTextFile({}).Close\r\n\
         Dim started As Double, elapsed As Double\r\n\
         started = Timer\r\n\
         Do\r\n\
         elapsed = Timer - started\r\n\
         If elapsed < 0 Then elapsed = elapsed + 86400\r\n\
         Loop While elapsed < 4\r\n\
         files.CreateTextFile({}).Close\r\n\
         End Sub\r\n",
            vba_path(&entered),
            vba_path(&finished)
        ),
    )
    .unwrap();
    std::fs::write(
        &queued_file,
        format!(
            "Sub main()\r\nCreateObject(\"Scripting.FileSystemObject\").CreateTextFile({}).Close\r\nEnd Sub\r\n",
            vba_path(&launched)
        ),
    )
    .unwrap();

    let addin = AddinClient::new();
    let mut holder_client = addin.client().unwrap();
    let mut holder_request = tonic::Request::new(proto::RunMacroRequest {
        path: holder_file.to_string_lossy().into_owned(),
    });
    holder_request.set_timeout(Duration::from_secs(10));
    let holder = tokio::spawn(async move { holder_client.run_macro(holder_request).await });
    let wait_started = Instant::now();
    while !entered.exists() && wait_started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if !entered.exists() {
        let result = holder.await.unwrap();
        panic!(
            "holder did not enter; RPC result: {result:?}; evidence: {}",
            folder.display()
        );
    }

    let mut queued_client = addin.client().unwrap();
    let mut queued_request = tonic::Request::new(proto::RunMacroRequest {
        path: queued_file.to_string_lossy().into_owned(),
    });
    queued_request.set_timeout(Duration::from_millis(250));
    let started = Instant::now();
    let queued_status = queued_client.run_macro(queued_request).await.unwrap_err();
    eprintln!(
        "Queued macro: {:?} after {:?}",
        queued_status.code(),
        started.elapsed()
    );
    assert!(matches!(
        queued_status.code(),
        tonic::Code::Cancelled | tonic::Code::DeadlineExceeded
    ));
    assert!(
        !finished.exists(),
        "holder must still own the lock when the queued macro expires"
    );

    let mut inspection_client = addin.client().unwrap();
    let mut inspection_request = tonic::Request::new(proto::InspectModelRequest {
        include_components: true,
        ..Default::default()
    });
    inspection_request.set_timeout(Duration::from_millis(250));
    let started = Instant::now();
    let inspection_status = inspection_client
        .inspect_model(inspection_request)
        .await
        .unwrap_err();
    eprintln!(
        "Queued inspection: {:?} after {:?}",
        inspection_status.code(),
        started.elapsed()
    );
    assert!(matches!(
        inspection_status.code(),
        tonic::Code::Cancelled | tonic::Code::DeadlineExceeded
    ));
    assert!(
        !finished.exists(),
        "holder must still own the lock when inspection expires"
    );

    let mut mesh_client = addin.client().unwrap();
    let mut mesh_request = tonic::Request::new(proto::GetMeshRequest {
        include_edges: true,
        ..Default::default()
    });
    mesh_request.set_timeout(Duration::from_millis(250));
    let started = Instant::now();
    let mesh_status = mesh_client.get_mesh(mesh_request).await.unwrap_err();
    eprintln!(
        "Queued mesh: {:?} after {:?}",
        mesh_status.code(),
        started.elapsed()
    );
    assert!(matches!(
        mesh_status.code(),
        tonic::Code::Cancelled | tonic::Code::DeadlineExceeded
    ));
    assert!(
        !finished.exists(),
        "holder must still own the lock when the mesh expires"
    );

    assert!(holder.await.unwrap().unwrap().into_inner().ran);
    assert!(finished.exists(), "holder must complete normally");
    // Let waiters resume after the holder releases the mutex. Checking the
    // launch marker immediately would race a buggy handler's late dispatch.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // A fresh macro acquires the same lock after expired handlers had a chance
    // to run. The marker must still be absent; a timeout alone cannot prove that.
    // A late mesh on a detailed assembly would block this barrier. Use a
    // shorter deadline than the ordinary 60 s request budget to detect it.
    let mut barrier_client = addin.client().unwrap();
    let mut barrier_request = tonic::Request::new(proto::RunMacroRequest {
        path: barrier_file.to_string_lossy().into_owned(),
    });
    barrier_request.set_timeout(Duration::from_secs(2));
    assert!(
        barrier_client
            .run_macro(barrier_request)
            .await
            .unwrap()
            .into_inner()
            .ran
    );
    assert!(
        !launched.exists(),
        "expired queued macro launched after its deadline"
    );
    // Positive control: the exact same queued script runs under a fresh request.
    assert!(
        addin
            .run_macro(proto::RunMacroRequest {
                path: queued_file.to_string_lossy().into_owned(),
            })
            .await
            .unwrap()
            .ran
    );
    assert!(
        launched.exists(),
        "positive control must create the launch marker"
    );
    eprintln!("Expired macro never launched; a fresh request ran successfully.");
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
#[ignore = "requires patched add-in and an active detailed model whose mesh takes more than one second; run without other clients"]
async fn expired_mesh_releases_model_lock() {
    let addin = AddinClient::new();
    let mut client = addin.client().unwrap();
    let control = || {
        let mut request = tonic::Request::new(proto::InspectModelRequest {
            include_components: true,
            include_bodies: true,
            ..Default::default()
        });
        request.set_timeout(Duration::from_secs(10));
        request
    };
    // Establish that geometry inspection works before starting a render.
    let before = client.inspect_model(control()).await.unwrap().into_inner();
    assert!(
        before.document.is_some(),
        "open a detailed part or assembly first"
    );
    assert!(
        !before.bodies.is_empty() || !before.components.is_empty(),
        "the active document must contain geometry"
    );

    let mut request = tonic::Request::new(proto::GetMeshRequest {
        include_edges: true,
        ..Default::default()
    });
    request.set_timeout(Duration::from_secs(1));
    let started = Instant::now();
    let status = client
        .get_mesh(request)
        .await
        .expect_err("use a model whose mesh takes more than one second");
    eprintln!(
        "Mesh expired: {:?} after {:?}",
        status.code(),
        started.elapsed()
    );
    assert!(matches!(
        status.code(),
        tonic::Code::Cancelled | tonic::Code::DeadlineExceeded
    ));

    // A real geometry request must recover without restarting SolidWorks.
    // Basic document queries now share the same execution gate too.
    let started = Instant::now();
    let after = client.inspect_model(control()).await.unwrap().into_inner();
    eprintln!(
        "Geometry inspection recovered after {:?}",
        started.elapsed()
    );
    assert_eq!(after.document, before.document);
    assert_eq!(after.components.len(), before.components.len());
    assert_eq!(after.bodies.len(), before.bodies.len());
}
