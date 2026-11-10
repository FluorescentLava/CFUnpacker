#![cfg_attr(windows, windows_subsystem = "windows")]

mod application;
mod bootstrap;

use cfunpacker::{
    bundle,
    detector::{self, Detection},
    engine::{self, Progress, Stats},
    profile::Game,
};
use naui_winui3::{
    IWindowNative,
    Microsoft::UI::{
        Dispatching::DispatcherQueueTimer,
        Xaml::{
            self, Application, ApplicationInitializationCallback, Controls::*, DragEventArgs,
            DragEventHandler, DragOperationDeferral, FrameworkElement, Markup::XamlReader,
            Media::MicaBackdrop, RoutedEventHandler, Visibility, Window,
        },
    },
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    marker::PhantomData,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};
use windows::{
    ApplicationModel::DataTransfer::{DataPackageOperation, StandardDataFormats},
    Foundation::{PropertyValue, TimeSpan, TypedEventHandler},
    Storage::IStorageItem,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, WPARAM},
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
            LibraryLoader::GetModuleHandleW,
        },
        UI::{
            HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
            Shell::{
                FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
                ITaskbarList3, SIGDN_FILESYSPATH, TBPF_NOPROGRESS, TBPF_NORMAL, TaskbarList,
            },
            WindowsAndMessaging::{
                HICON, ICON_BIG, ICON_SMALL, IMAGE_ICON, LoadImageW, MB_ICONERROR, MB_OK,
                MessageBoxW, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SendMessageW,
                WM_SETICON,
            },
        },
    },
    core::{HSTRING, Interface, Owned, PCWSTR, Result, w},
};
use windows_collections::IVectorView;
use windows_future::{AsyncOperationCompletedHandler, IAsyncOperation};

// The dependency's generated projection omits Storyboard. This interface and
// slot order come from Microsoft.UI.Xaml.winmd (IStoryboard, WinUI 2.3.0).
windows_core::imp::define_interface!(
    ResultStoryboard,
    ResultStoryboardVtbl,
    0x04d41bb3_8721_519e_8e53_fb8b34920305
);

#[repr(C)]
pub struct ResultStoryboardVtbl {
    base: windows_core::IInspectable_Vtbl,
    children: usize,
    seek: usize,
    stop: unsafe extern "system" fn(*mut std::ffi::c_void) -> windows_core::HRESULT,
    begin: unsafe extern "system" fn(*mut std::ffi::c_void) -> windows_core::HRESULT,
    pause: usize,
    resume: usize,
    current_state: usize,
    current_time: usize,
    seek_aligned_to_last_tick: usize,
    skip_to_fill: usize,
}

impl ResultStoryboard {
    fn begin(&self) -> Result<()> {
        // SAFETY: self owns a queried IStoryboard with the SDK's ABI above.
        unsafe { (self.vtable().begin)(self.as_raw()).ok() }
    }

    fn stop(&self) -> Result<()> {
        // SAFETY: self owns a queried IStoryboard with the SDK's ABI above.
        unsafe { (self.vtable().stop)(self.as_raw()).ok() }
    }
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
    static APP: RefCell<Option<Application>> = const { RefCell::new(None) };
}
fn with_ui(action: impl FnOnce(&Ui) -> Result<()>) -> Result<()> {
    let ui = UI.with(|slot| slot.borrow().clone());
    if let Some(ui) = ui {
        action(&ui)
    } else {
        Ok(())
    }
}
fn h(value: impl AsRef<str>) -> HSTRING {
    HSTRING::from(value.as_ref())
}

fn set_embedded_window_icons(hwnd: HWND) -> Result<[Owned<HICON>; 2]> {
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let load = |width, height| -> Result<Owned<HICON>> {
        // build.rs embeds AppIcon.ico as icon resource 1.
        unsafe {
            Ok(Owned::new(HICON(
                LoadImageW(
                    Some(instance),
                    PCWSTR(std::ptr::without_provenance(1)),
                    IMAGE_ICON,
                    GetSystemMetricsForDpi(width, dpi),
                    GetSystemMetricsForDpi(height, dpi),
                    Default::default(),
                )?
                .0,
            )))
        }
    };
    let icons = [load(SM_CXICON, SM_CYICON)?, load(SM_CXSMICON, SM_CYSMICON)?];
    for (kind, icon) in [(ICON_BIG, &icons[0]), (ICON_SMALL, &icons[1])] {
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(kind as usize)),
                Some(LPARAM((**icon).0 as isize)),
            );
        }
    }
    Ok(icons)
}

struct Pending {
    apk: PathBuf,
    output: PathBuf,
    selected: Option<Game>,
    overwrite: bool,
}
enum Event {
    Detected(std::result::Result<Detection, String>, Pending),
    Choice(i32, Pending, Option<Game>),
    Progress(Progress),
    Finished(std::result::Result<Stats, String>),
    ProgressClosed,
    DropReady(u64),
}

// WinUI deferrals belong to the UI apartment. Keep them here, never in an
// async completion delegate, which WinRT may invoke on a worker thread.
struct DropDeferral {
    inner: Option<DragOperationDeferral>,
    // The generated WinUI projection marks this type Send. Override that here
    // so moving the deferral into a worker delegate is a compile-time error.
    _apartment: PhantomData<Rc<()>>,
}

impl DropDeferral {
    fn new(deferral: DragOperationDeferral) -> Self {
        Self {
            inner: Some(deferral),
            _apartment: PhantomData,
        }
    }

    fn complete(mut self) -> Result<()> {
        self.inner.take().unwrap().Complete()
    }
}

impl Drop for DropDeferral {
    fn drop(&mut self) {
        if let Some(deferral) = self.inner.take() {
            let _ = deferral.Complete();
        }
    }
}

struct PendingDrop {
    deferral: DropDeferral,
    operation: IAsyncOperation<IVectorView<IStorageItem>>,
}
#[derive(Default)]
struct Model {
    busy: bool,
    close_requested: bool,
    awaiting_choice: bool,
    result_pending: bool,
    selected_apk: Option<PathBuf>,
    last_output: Option<PathBuf>,
    log: String,
    last_message: String,
}
struct Ui {
    window: Window,
    _icons: [Owned<HICON>; 2],
    page: FrameworkElement,
    hwnd: HWND,
    game: ComboBox,
    input: TextBox,
    output: TextBox,
    overwrite: CheckBox,
    start: Button,
    browse_input: Button,
    browse_output: Button,
    show_log: Button,
    open_output: Button,
    drop_target: Border,
    drop_hint: TextBlock,
    result: InfoBar,
    result_animation: ResultStoryboard,
    progress_dialog: ContentDialog,
    status: TextBlock,
    progress_bar: ProgressBar,
    percent: TextBlock,
    mismatch: ContentDialog,
    mismatch_text: TextBlock,
    log_dialog: ContentDialog,
    log_text: TextBox,
    timer: DispatcherQueueTimer,
    taskbar: Option<ITaskbarList3>,
    model: RefCell<Model>,
    cancel: Arc<AtomicBool>,
    sender: Sender<Event>,
    receiver: RefCell<Receiver<Event>>,
    pending_drops: RefCell<HashMap<u64, PendingDrop>>,
    next_drop: Cell<u64>,
}

impl Ui {
    fn launch() -> Result<()> {
        let window = Window::new()?;
        window.SetTitle(&h("CFUnpacker"))?;
        window.SetSystemBackdrop(&MicaBackdrop::new()?)?;
        let layout: FrameworkElement =
            XamlReader::Load(&h(include_str!("../ui/MainWindow.xaml")))?.cast()?;
        window.SetContent(&layout)?;
        window.SetExtendsContentIntoTitleBar(true)?;
        let title: Xaml::UIElement = layout.FindName(&h("AppTitleBar"))?.cast()?;
        window.SetTitleBar(&title)?;
        let page: FrameworkElement = layout.FindName(&h("MainPage"))?.cast()?;
        fn find<T: Interface>(page: &FrameworkElement, name: &str) -> Result<T> {
            page.FindName(&h(name))?.cast()
        }
        let hwnd = unsafe { window.cast::<IWindowNative>()?.WindowHandle()? };
        let app_window = window.AppWindow()?;
        app_window.Resize(windows::Graphics::SizeInt32 {
            Width: 900,
            Height: 1120,
        })?;
        let icons = set_embedded_window_icons(hwnd)?;
        let timer = window.DispatcherQueue()?.CreateTimer()?;
        timer.SetInterval(TimeSpan { Duration: 500_000 })?;
        let taskbar: Option<ITaskbarList3> =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok() };
        if let Some(taskbar) = &taskbar {
            let _ = unsafe { taskbar.HrInit() };
        }
        let (sender, receiver) = mpsc::channel();
        let ui = Rc::new(Self {
            game: find(&page, "GameComboBox")?,
            input: find(&page, "InputFolderTextBox")?,
            output: find(&page, "OutputPathTextBox")?,
            overwrite: find(&page, "OverwriteCheckBox")?,
            start: find(&page, "StartButton")?,
            browse_input: find(&page, "BrowseInputFolderButton")?,
            browse_output: find(&page, "BrowseOutputButton")?,
            show_log: find(&page, "ShowLogButton")?,
            open_output: find(&page, "OpenOutputButton")?,
            drop_target: find(&page, "ApkDropTarget")?,
            drop_hint: find(&page, "DropHintText")?,
            result: find(&page, "ResultInfoBar")?,
            result_animation: page
                .Resources()?
                .Lookup(&PropertyValue::CreateString(&h("ResultEntrance"))?)?
                .cast()?,
            progress_dialog: find(&page, "ProgressDialog")?,
            status: find(&page, "StatusTextBlock")?,
            progress_bar: find(&page, "UnpackProgressBar")?,
            percent: find(&page, "ProgressTextBlock")?,
            mismatch: find(&page, "ProfileMismatchDialog")?,
            mismatch_text: find(&page, "ProfileMismatchText")?,
            log_dialog: find(&page, "LogDialog")?,
            log_text: find(&page, "LogTextBox")?,
            window,
            _icons: icons,
            page,
            hwnd,
            timer,
            taskbar,
            model: RefCell::new(Model::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            sender,
            receiver: RefCell::new(receiver),
            pending_drops: RefCell::new(HashMap::new()),
            next_drop: Cell::new(0),
        });
        UI.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&ui)));
        ui.wire_events()?;
        ui.timer.Start()?;
        ui.window.Activate()?;
        Ok(())
    }

    fn wire_events(&self) -> Result<()> {
        self.start
            .Click(&RoutedEventHandler::new(|_, _| with_ui(|ui| ui.begin())))?;
        self.browse_input.Click(&RoutedEventHandler::new(|_, _| {
            with_ui(|ui| {
                if let Some(path) = ui.pick_folder()? {
                    ui.model.borrow_mut().selected_apk = None;
                    ui.input.SetText(&h(path.to_string_lossy()))?;
                    if ui.output.Text()?.is_empty() {
                        ui.output.SetText(&h(path.to_string_lossy()))?;
                    }
                }
                Ok(())
            })
        }))?;
        self.browse_output.Click(&RoutedEventHandler::new(|_, _| {
            with_ui(|ui| {
                if let Some(path) = ui.pick_folder()? {
                    ui.output.SetText(&h(path.to_string_lossy()))?;
                }
                Ok(())
            })
        }))?;
        self.open_output.Click(&RoutedEventHandler::new(|_, _| {
            with_ui(|ui| {
                if let Some(path) = &ui.model.borrow().last_output {
                    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
                }
                Ok(())
            })
        }))?;
        self.show_log.Click(&RoutedEventHandler::new(|_, _| {
            with_ui(|ui| {
                ui.log_dialog.SetXamlRoot(&ui.page.XamlRoot()?)?;
                ui.log_text.SetText(&h(&ui.model.borrow().log))?;
                ui.log_dialog.ShowAsync()?;
                Ok(())
            })
        }))?;
        self.result
            .CloseButtonClick(&TypedEventHandler::new(|_, _| {
                with_ui(|ui| ui.hide_result())
            }))?;
        self.progress_dialog.Closing(&TypedEventHandler::<
            ContentDialog,
            ContentDialogClosingEventArgs,
        >::new(|_, args| {
            let args = args.as_ref().unwrap();
            with_ui(|ui| {
                if ui.model.borrow().busy {
                    args.SetCancel(true)?;
                    ui.cancel.store(true, Ordering::Relaxed);
                    ui.status.SetText(&h("正在取消并清理暂存文件…"))?;
                }
                Ok(())
            })
        }))?;
        self.drop_target
            .DragOver(&DragEventHandler::new(|_, args| {
                let args = args.as_ref().unwrap();
                with_ui(|ui| {
                    if !ui.model.borrow().busy {
                        args.SetAcceptedOperation(DataPackageOperation::Copy)?;
                        ui.drop_hint.SetText(&h("松开以选择 APK"))?;
                    }
                    Ok(())
                })
            }))?;
        self.drop_target
            .DragLeave(&DragEventHandler::new(|_, _| with_ui(|ui| ui.reset_drop())))?;
        self.drop_target.Drop(&DragEventHandler::new(|_, args| {
            with_ui(|ui| {
                let result = ui.begin_drop(args.as_ref().unwrap());
                ui.report_drop_error(result)
            })
        }))?;
        self.timer
            .Tick(&TypedEventHandler::new(|_, _| with_ui(|ui| ui.poll())))?;
        self.window.AppWindow()?.Closing(&TypedEventHandler::<
            _,
            naui_winui3::Microsoft::UI::Windowing::AppWindowClosingEventArgs,
        >::new(|_, args| {
            with_ui(|ui| {
                if ui.model.borrow().busy {
                    args.as_ref().unwrap().SetCancel(true)?;
                    ui.model.borrow_mut().close_requested = true;
                    ui.cancel.store(true, Ordering::Relaxed);
                    ui.status.SetText(&h("正在清理暂存文件，完成后关闭…"))?;
                    if ui.model.borrow().awaiting_choice {
                        ui.mismatch.Hide()?;
                    }
                }
                Ok(())
            })
        }))?;
        Ok(())
    }

    fn reset_drop(&self) -> Result<()> {
        self.drop_hint
            .SetText(&h("拖入 .apk / .apks / .xapk（支持 .apk.1）"))
    }
    fn begin_drop(&self, args: &DragEventArgs) -> Result<()> {
        self.reset_drop()?;
        if self.model.borrow().busy {
            return Ok(());
        }
        let data = args.DataView()?;
        if !data.Contains(&StandardDataFormats::StorageItems()?)? {
            return self.error("请拖入 .apk / .apks / .xapk 文件（支持 .apk.1）。");
        }
        let deferral = DropDeferral::new(args.GetDeferral()?);
        let operation = data.GetStorageItemsAsync()?;
        let id = self.next_drop.get();
        self.next_drop.set(id.wrapping_add(1));
        self.pending_drops.borrow_mut().insert(
            id,
            PendingDrop {
                deferral,
                operation: operation.clone(),
            },
        );
        let sender = self.sender.clone();
        let result = operation.SetCompleted(&AsyncOperationCompletedHandler::new(move |_, _| {
            // Only send a notification here. Both GetResults and Complete run
            // on the UI thread when its dispatcher timer drains the channel.
            let _ = sender.send(Event::DropReady(id));
            Ok(())
        }));
        if result.is_err() {
            self.pending_drops.borrow_mut().remove(&id);
        }
        result
    }
    fn finish_drop(&self, id: u64) -> Result<()> {
        let pending = self.pending_drops.borrow_mut().remove(&id);
        let Some(pending) = pending else {
            return Ok(());
        };
        let items = pending.operation.GetResults();
        pending.deferral.complete()?;
        let items = items?;
        if self.model.borrow().busy {
            return Ok(());
        }
        for item in items {
            let path = PathBuf::from(item.Path()?.to_string());
            if path.is_file() && bundle::is_supported_input(&path) {
                self.input
                    .SetText(&h(path.parent().unwrap_or(Path::new("")).to_string_lossy()))?;
                self.drop_hint
                    .SetText(&h(path.file_name().unwrap_or_default().to_string_lossy()))?;
                if self.output.Text()?.is_empty() {
                    self.output.SetText(&self.input.Text()?)?;
                }
                self.model.borrow_mut().selected_apk = Some(path);
                return Ok(());
            }
        }
        self.error("请拖入 .apk / .apks / .xapk 文件（支持 .apk.1）。")
    }
    fn report_drop_error(&self, result: Result<()>) -> Result<()> {
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = format!("无法读取拖入的文件：{error}");
                self.log(&message);
                self.error(&message)
            }
        }
    }
    fn pick_folder(&self) -> Result<Option<PathBuf>> {
        unsafe {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            dialog.SetOptions(dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)?;
            if let Err(error) = dialog.Show(Some(self.hwnd)) {
                if error.code().0 as u32 == 0x800704c7 {
                    return Ok(None);
                }
                return Err(error);
            }
            let path = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let text = path.to_string();
            CoTaskMemFree(Some(path.0.cast()));
            Ok(Some(PathBuf::from(text?)))
        }
    }
    fn set_busy(&self, value: bool) -> Result<()> {
        self.model.borrow_mut().busy = value;
        self.game.SetIsEnabled(!value)?;
        self.input.SetIsEnabled(!value)?;
        self.output.SetIsEnabled(!value)?;
        self.browse_input.SetIsEnabled(!value)?;
        self.browse_output.SetIsEnabled(!value)?;
        self.overwrite.SetIsEnabled(!value)?;
        self.start.SetIsEnabled(!value)?;
        self.show_log.SetIsEnabled(!value)?;
        if let Some(taskbar) = &self.taskbar {
            let _ = unsafe {
                taskbar
                    .SetProgressState(self.hwnd, if value { TBPF_NORMAL } else { TBPF_NOPROGRESS })
            };
        }
        Ok(())
    }
    fn error(&self, message: &str) -> Result<()> {
        self.result.SetSeverity(InfoBarSeverity::Error)?;
        self.result.SetTitle(&h("无法开始解包"))?;
        self.result.SetMessage(&h(message))?;
        self.show_result()
    }
    fn hide_result(&self) -> Result<()> {
        self.model.borrow_mut().result_pending = false;
        self.result.SetIsOpen(false)?;
        self.result.SetVisibility(Visibility::Collapsed)?;
        self.result_animation.stop()
    }
    fn show_result(&self) -> Result<()> {
        self.result_animation.stop()?;
        self.result.SetOpacity(0.0)?;
        self.result.SetIsOpen(true)?;
        self.result.SetVisibility(Visibility::Visible)?;
        self.result.UpdateLayout()?;
        self.result_animation.begin()
    }
    fn log(&self, message: &str) {
        let mut model = self.model.borrow_mut();
        if model.last_message != message {
            model.log.push_str(message);
            model.log.push_str("\r\n");
            model.last_message = message.to_owned();
        }
    }
    fn resolve_apk(&self) -> anyhow::Result<PathBuf> {
        let text = self.input.Text()?.to_string();
        let path = PathBuf::from(text.trim().trim_matches('"'));
        if path.is_file() && bundle::is_supported_input(&path) {
            return Ok(path);
        }
        anyhow::ensure!(path.is_dir(), "请选择有效的输入文件夹，或输入 APK 路径。");
        if let Some(apk) = self
            .model
            .borrow()
            .selected_apk
            .as_ref()
            .filter(|apk| apk.is_file() && apk.parent() == Some(path.as_path()))
        {
            return Ok(apk.clone());
        }
        let mut candidates = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let path = entry?.path();
            if path.is_file() && bundle::is_supported_input(&path) {
                candidates.push(path);
                if candidates.len() == 2 {
                    break;
                }
            }
        }
        anyhow::ensure!(
            candidates.len() == 1,
            if candidates.is_empty() {
                "输入文件夹中没有 .apk / .apks / .xapk 文件。"
            } else {
                "输入文件夹中有多个 APK，请拖入要解包的文件。"
            }
        );
        Ok(candidates.remove(0))
    }
    fn begin(&self) -> Result<()> {
        if self.model.borrow().busy {
            return Ok(());
        }
        let apk = match self.resolve_apk() {
            Ok(apk) => apk,
            Err(error) => return self.error(&format!("{error:#}")),
        };
        let output = self.output.Text()?.to_string();
        if output.trim().is_empty() {
            return self.error("请选择输出文件夹。");
        }
        let index = self.game.SelectedIndex()?;
        let selected = if index > 0 {
            Game::ALL.get(index as usize - 1).copied()
        } else {
            None
        };
        let overwrite = self.overwrite.IsChecked()?.Value()?;
        let pending = Pending {
            apk,
            output: PathBuf::from(output.trim().trim_matches('"')),
            selected,
            overwrite,
        };
        self.hide_result()?;
        self.open_output.SetIsEnabled(false)?;
        {
            let mut model = self.model.borrow_mut();
            model.log.clear();
            model.last_message.clear();
            model.last_output = None;
        }
        self.cancel.store(false, Ordering::Relaxed);
        self.set_busy(true)?;
        let sender = self.sender.clone();
        let cancel = Arc::clone(&self.cancel);
        std::thread::spawn(move || {
            let detection = detector::detect(&pending.apk, &cancel).map_err(|e| format!("{e:#}"));
            let _ = sender.send(Event::Detected(detection, pending));
        });
        Ok(())
    }
    fn detected(
        &self,
        result: std::result::Result<Detection, String>,
        pending: Pending,
    ) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            self.set_busy(false)?;
            return self.maybe_close();
        }
        let detection = match result {
            Ok(value) => value,
            Err(error) => {
                self.set_busy(false)?;
                return self.error(&error);
            }
        };
        self.log(&format!(
            "APK 识别：{} ({:.0} ms)",
            detection.evidence,
            detection.elapsed_seconds * 1000.0
        ));
        let detected = detection.game.and_then(Game::from_id);
        if let Some(selected) = pending.selected {
            if !detection.compatible.contains(&selected.id()) {
                self.mismatch.SetXamlRoot(&self.page.XamlRoot()?)?;
                if let Some(game) = detected {
                    self.mismatch
                        .SetTitle(&PropertyValue::CreateString(&h("APK 类型不一致"))?)?;
                    self.mismatch.SetPrimaryButtonText(&h("切换并解包"))?;
                    self.mismatch.SetSecondaryButtonText(&h("强制执行"))?;
                    self.mismatch_text.SetText(&h(format!(
                        "当前选择为“{}”，但资源识别结果为“{}”。\n\n{}",
                        selected.display_name(),
                        game.display_name(),
                        detection.evidence
                    )))?;
                } else {
                    self.mismatch
                        .SetTitle(&PropertyValue::CreateString(&h("无法确认 APK 类型"))?)?;
                    self.mismatch.SetPrimaryButtonText(&h("强制解包"))?;
                    self.mismatch.SetSecondaryButtonText(&HSTRING::new())?;
                    self.mismatch_text.SetText(&h(format!(
                        "未能验证该 APK 与“{}”匹配。\n\n{}",
                        selected.display_name(),
                        detection.evidence
                    )))?;
                }
                let sender = self.sender.clone();
                self.model.borrow_mut().awaiting_choice = true;
                // Delegates can outlive this stack and run on another COM thread.
                let pending = Arc::new(std::sync::Mutex::new(Some(pending)));
                self.mismatch
                    .ShowAsync()?
                    .SetCompleted(&AsyncOperationCompletedHandler::new(move |operation, _| {
                        let result = operation
                            .as_ref()
                            .and_then(|op| op.GetResults().ok())
                            .unwrap_or(ContentDialogResult::None);
                        if let Some(pending) = pending.lock().unwrap().take() {
                            let _ = sender.send(Event::Choice(result.0, pending, detected));
                        }
                        Ok(())
                    }))?;
                return Ok(());
            }
            self.run(pending, selected)
        } else if let Some(game) = detected {
            self.run(pending, game)
        } else {
            self.set_busy(false)?;
            self.error(&format!(
                "无法自动识别 APK 类型。{} 请手动选择版本后再试。",
                detection.evidence
            ))
        }
    }
    fn run(&self, pending: Pending, game: Game) -> Result<()> {
        self.progress_dialog.SetXamlRoot(&self.page.XamlRoot()?)?;
        self.status.SetText(&h("正在准备…"))?;
        self.progress_bar.SetValue(0.0)?;
        self.percent.SetText(&h("0%"))?;
        let closed_sender = self.sender.clone();
        self.progress_dialog
            .ShowAsync()?
            .SetCompleted(&AsyncOperationCompletedHandler::new(move |_, _| {
                let _ = closed_sender.send(Event::ProgressClosed);
                Ok(())
            }))?;
        let sender = self.sender.clone();
        let cancel = Arc::clone(&self.cancel);
        self.log(&format!("按“{}”解包。", game.display_name()));
        std::thread::spawn(move || {
            let result = engine::unpack(
                &pending.apk,
                &pending.output,
                game,
                pending.overwrite,
                engine::default_workers(),
                &cancel,
                &|p| {
                    let _ = sender.send(Event::Progress(p));
                },
            )
            .map_err(|e| format!("{e:#}"));
            let _ = sender.send(Event::Finished(result));
        });
        Ok(())
    }
    fn poll(&self) -> Result<()> {
        // Drain without holding the receiver/model borrow across XAML callbacks.
        let events: Vec<_> = self.receiver.borrow().try_iter().collect();
        for event in events {
            match event {
                Event::Detected(result, pending) => self.detected(result, pending)?,
                Event::Choice(choice, pending, detected) => {
                    self.model.borrow_mut().awaiting_choice = false;
                    if self.cancel.load(Ordering::Relaxed) {
                        self.set_busy(false)?;
                        self.maybe_close()?;
                        continue;
                    }
                    if choice == ContentDialogResult::Primary.0 {
                        let game = detected.or(pending.selected).unwrap();
                        self.game.SetSelectedIndex(
                            Game::ALL.iter().position(|g| *g == game).unwrap() as i32 + 1,
                        )?;
                        self.run(pending, game)?;
                    } else if choice == ContentDialogResult::Secondary.0 {
                        let game = pending.selected.unwrap();
                        self.run(pending, game)?;
                    } else {
                        self.set_busy(false)?;
                        self.maybe_close()?;
                    }
                }
                Event::Progress(p) => {
                    self.progress_bar.SetValue(p.percent)?;
                    self.percent.SetText(&h(format!("{:.0}%", p.percent)))?;
                    if !self.cancel.load(Ordering::Relaxed) {
                        self.status.SetText(&h(&p.message))?;
                    }
                    self.log(&p.message);
                    if let Some(taskbar) = &self.taskbar {
                        let _ = unsafe {
                            taskbar.SetProgressValue(self.hwnd, (p.percent * 10.0) as u64, 1000)
                        };
                    }
                }
                Event::Finished(result) => self.finished(result)?,
                Event::ProgressClosed => {
                    if self.model.borrow().result_pending {
                        self.model.borrow_mut().result_pending = false;
                        self.show_result()?;
                    }
                }
                Event::DropReady(id) => {
                    self.report_drop_error(self.finish_drop(id))?;
                }
            }
        }
        Ok(())
    }
    fn finished(&self, result: std::result::Result<Stats, String>) -> Result<()> {
        self.set_busy(false)?;
        match result {
            Ok(stats) => {
                self.result.SetSeverity(InfoBarSeverity::Success)?;
                self.result.SetTitle(&h(format!(
                    "解包完成，用时 {:.2} 秒",
                    stats.elapsed_seconds
                )))?;
                self.result.SetMessage(&HSTRING::new())?;
                self.log(&format!(
                    "完成：提取 {} 个资源，拆分 {} 个图集，就地解密 {} 张独立 PNG，跳过 {} 项。",
                    stats.assets_extracted,
                    stats.atlases_decoded,
                    stats.decrypted_standalone_pngs,
                    stats.skipped_items
                ));
                for warning in &stats.warnings {
                    self.log(&format!("警告：{warning}"));
                }
                self.model.borrow_mut().last_output = Some(PathBuf::from(stats.output_path));
                self.open_output.SetIsEnabled(true)?;
            }
            Err(error) if self.cancel.load(Ordering::Relaxed) => {
                self.result.SetSeverity(InfoBarSeverity::Warning)?;
                self.result.SetTitle(&h("已取消"))?;
                self.result
                    .SetMessage(&h("暂存文件已清理，未生成或替换最终输出目录。"))?;
                self.log(&error);
            }
            Err(error) => {
                self.result.SetSeverity(InfoBarSeverity::Error)?;
                self.result.SetTitle(&h("解包失败"))?;
                self.result.SetMessage(&h(&error))?;
                self.log(&error);
            }
        }
        // Let the modal finish its native closing animation before the result
        // enters; otherwise the dialog covers most of the entrance transition.
        self.model.borrow_mut().result_pending = true;
        self.progress_dialog.Hide()?;
        self.maybe_close()
    }
    fn maybe_close(&self) -> Result<()> {
        if self.model.borrow().close_requested {
            self.window.Close()?;
        }
        Ok(())
    }
}

fn run() -> Result<()> {
    naui_winui3::init_apartment(naui_winui3::ApartmentType::SingleThreaded)?;
    Application::Start(&ApplicationInitializationCallback::new(|_| {
        let app = application::create(|| {
            let initialize = || -> Result<()> {
                let app = Application::Current()?;
                let fluent: Xaml::ResourceDictionary = XamlControlsResources::new()?.cast()?;
                app.Resources()?.MergedDictionaries()?.Append(&fluent)?;
                let markup = include_str!("../ui/Resources.xaml").replace(
                    "<XamlControlsResources xmlns=\"using:Microsoft.UI.Xaml.Controls\" />",
                    "",
                );
                let resources: Xaml::ResourceDictionary = XamlReader::Load(&h(markup))?.cast()?;
                app.Resources()?.MergedDictionaries()?.Append(&resources)?;
                Ui::launch()
            };
            if let Err(error) = initialize() {
                show_startup_error(&error);
                Application::Current()?.Exit()?;
            }
            Ok(())
        })?;
        APP.with(|slot| *slot.borrow_mut() = Some(app));
        Ok(())
    }))?;
    UI.with(|slot| *slot.borrow_mut() = None);
    APP.with(|slot| *slot.borrow_mut() = None);
    Ok(())
}
fn show_startup_error(error: &windows::core::Error) {
    let text = h(format!(
        "无法启动 CFUnpacker：{error:?}\n\n请确认已安装 Windows App Runtime 2.3.1 或更新的兼容版本。"
    ));
    unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(text.as_ptr()),
            w!("CFUnpacker"),
            MB_OK | MB_ICONERROR,
        );
    }
}
fn main() {
    let _bootstrap = match bootstrap::initialize() {
        Ok(value) => value,
        Err(error) => {
            error.show_dialog();
            return;
        }
    };
    if let Err(error) = run() {
        show_startup_error(&error);
    }
}
