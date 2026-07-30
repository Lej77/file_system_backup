use crate::{
    CancelSignal, Result,
    logging::CommonOpt,
    winfsp_memfs::{WinFspMemFs, WinFspMemFsContext},
};
use clap::Args;
use color_eyre::eyre::Context;

#[derive(Debug, Args, Clone)]
pub struct TestWinFspOpts {
    #[clap(flatten)]
    pub common: CommonOpt,
}
impl TestWinFspOpts {
    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        winfsp::winfsp_init().map_err(|e| {
            let report = color_eyre::eyre::eyre!("{e:?}\n{e}");
            if let winfsp::FspError::WIN32(1285) = e {
                report.wrap_err(
                    "The error code corresponds to ERROR_DELAY_LOAD_FAILED which means we failed to load \
                    WinFsp's dynamically linked library (.dll), make sure that WinFsp is correctly installed."
                )
            } else {
                report
            }
        }).wrap_err("Failed to initialize WinFsp")?;
        let mut cx = WinFspMemFsContext::new();
        {
            let cx = std::sync::Arc::get_mut(&mut cx.shared)
                .unwrap()
                .get_mut()
                .unwrap();
            // Example files:
            let _ = cx.make_node(
                &winfsp::U16CString::from_str("test").unwrap(),
                false,
                Vec::new(),
            );
            let _ = cx.make_node(
                &winfsp::U16CString::from_str("a folder").unwrap(),
                true,
                Vec::new(),
            );
            let _ = cx.make_node(
                &winfsp::U16CString::from_str("a folder/hello_world.txt").unwrap(),
                false,
                b"Hello world!".to_vec(),
            );
        }
        let mut mem_fs = WinFspMemFs::create_host(cx)
            .wrap_err("Failed to create WinFsp MemFs file system host")?;
        mem_fs
            .fs
            .mount(winfsp::host::MountPoint::NextFreeDrive)
            .map_err(|e| color_eyre::eyre::eyre!("{} (HRESULT {})", e.message(), e.code()))
            .wrap_err("Failed to mount WinFsp file system")?;
        mem_fs
            .fs
            .start()
            .wrap_err("Failed to start WinFsp file system")?;

        //winfsp_memfs::WinFspMemFs::create_service(winfsp_memfs::CreationOptions {
        //    init_token: None,
        //    mount_point: "C:/WinFsp-MemFs-Test".into(),
        //    service_name: "test-WinFsp-MemFs".into(),
        //}).map_err(|e| {
        //    let report = color_eyre::eyre::eyre!("{e:?}\n{e}");
        //    if let winfsp::FspError::WIN32(1285) = e {
        //        report.wrap_err(
        //            "Underlying error code means ERROR_DELAY_LOAD_FAILED which means we failed to load \
        //            WinFsp's dynamically linked library (.dll), make sure that WinFsp is correctly installed."
        //        )
        //    } else {
        //        report
        //    }
        //}).wrap_err("failed to start WinFsp file system")?;

        log::info!("Press Ctrl+C to exit");
        loop {
            if cancel_signal
                .wait_timeout(std::time::Duration::from_millis(100))
                .is_err()
            {
                return Ok(());
            }
        }
    }
}
