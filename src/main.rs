use std::{
    error::Error,
    ffi::{c_int, c_void},
    process,
};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{
    AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent,
    NSFloatingWindowLevel, NSScreen, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{MainThreadMarker, NSError, NSObject, NSObjectProtocol, ns_string};
use objc2_quartz_core::{CAAutoresizingMask, CALayer, CATransaction, kCAGravityResize};
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType,
};
use rift_client::{ReactorCommand, RiftCommand, RiftMachClient, WindowId};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

unsafe extern "C" {
    safe fn SLSMainConnectionID() -> i32;
    safe fn SLSSetWindowTags(cid: i32, wid: u32, tags: *mut u64, count: c_int) -> i32;
    fn CMSampleBufferGetImageBuffer(sample: &CMSampleBuffer) -> *mut c_void;
    fn CVPixelBufferGetIOSurface(buffer: *mut c_void) -> *mut c_void;
}

const W: f64 = 480.;
const H: f64 = 480.;
const CLOSE: f64 = 22.;
const INSET: f64 = 10.;
const MARGIN: f64 = 20.;
const FPS: i32 = 30;

#[derive(Clone, Copy)]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

fn pip_size(w: f64, h: f64) -> CGSize {
    let (w, h) = if w.is_finite() && h.is_finite() && w > 0. && h > 0. {
        (w, h)
    } else {
        (W, 270.)
    };
    let scale = (W / w).min(H / h);
    CGSize::new(w * scale, h * scale)
}

fn spawn(v: CGRect, s: CGSize, c: Corner) -> CGRect {
    let x = match c {
        Corner::TopLeft | Corner::BottomLeft => v.origin.x + MARGIN,
        _ => v.origin.x + v.size.width - s.width - MARGIN,
    };
    let y = match c {
        Corner::TopLeft | Corner::TopRight => v.origin.y + v.size.height - s.height - MARGIN,
        _ => v.origin.y + MARGIN,
    };
    rect(x, y, s.width, s.height)
}

fn close_frame(b: CGRect) -> CGRect {
    rect(
        b.origin.x + INSET,
        b.origin.y + b.size.height - INSET - CLOSE,
        CLOSE,
        CLOSE,
    )
}

fn in_rect(r: CGRect, p: CGPoint) -> bool {
    p.x >= r.origin.x
        && p.x <= r.origin.x + r.size.width
        && p.y >= r.origin.y
        && p.y <= r.origin.y + r.size.height
}

fn no_animation(f: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    f();
    CATransaction::commit()
}

fn fatal(message: &str, error: *mut NSError) -> ! {
    let detail = unsafe { error.as_ref() }
        .map(|e| format!(": {}", e.localizedDescription()))
        .unwrap_or_default();
    eprintln!("{message}{detail}");
    process::exit(1)
}

struct PipIvars(RiftMachClient, WindowId, u32, Retained<CALayer>);

define_class!(
    #[unsafe(super(NSView, objc2_app_kit::NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PipIvars]
    struct PipView;

    unsafe impl NSObjectProtocol for PipView {}

    impl PipView {
        #[unsafe(method(mouseEntered:))]
        fn entered(&self, _: &NSEvent) { self.show_close(true) }

        #[unsafe(method(mouseExited:))]
        fn exited(&self, _: &NSEvent) { self.show_close(false) }

        #[unsafe(method(mouseUp:))]
        fn clicked(&self, event: &NSEvent) {
            if in_rect(close_frame(self.bounds()), event.locationInWindow()) { process::exit(0) }
            let i = self.ivars();
            if let Err(e) = i.0.execute_command(RiftCommand::Reactor(ReactorCommand::FocusWindow {
                window_id: i.1,
                window_server_id: Some(i.2),
            })) { eprintln!("could not focus the source window: {e}") }
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _: Option<&NSEvent>) -> bool { true }
    }
);

impl PipView {
    fn show_close(&self, show: bool) {
        no_animation(|| self.ivars().3.setHidden(!show))
    }
}

struct OutputIvars(Retained<CALayer>);

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn frame(&self, _: &SCStream, sample: &CMSampleBuffer, _: SCStreamOutputType) {
            let buffer = unsafe { CMSampleBufferGetImageBuffer(sample) };
            if buffer.is_null() {
                return;
            }
            let surface = unsafe { CVPixelBufferGetIOSurface(buffer) };
            if surface.is_null() {
                return;
            }
            no_animation(|| unsafe {
                self.ivars()
                    .0
                    .setContents(Some(&*(surface as *const AnyObject)))
            });
        }
    }
);

fn capture(wid: u32, layer: Retained<CALayer>, size: CGSize, scale: f64) {
    let listed = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if !error.is_null() {
                fatal(
                    "screen capture permission is required; enable your terminal in System Settings > Privacy & Security > Screen & System Audio Recording, then run again",
                    error,
                )
            }
            let Some(content) = (unsafe { content.as_ref() }) else {
                fatal("ScreenCaptureKit returned no shareable content", error)
            };
            let windows = unsafe { content.windows() };
            let Some(source) = (0..windows.count())
                .map(|i| windows.objectAtIndex(i))
                .find(|w| unsafe { w.windowID() } == wid)
            else {
                fatal(
                    "the focused window is not capturable; make sure it is still open and visible",
                    std::ptr::null_mut(),
                )
            };
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(
                    <SCContentFilter as AnyThread>::alloc(),
                    &source,
                )
            };
            let config = unsafe { SCStreamConfiguration::new() };
            unsafe {
                config.setWidth((size.width * scale).round().max(1.) as usize);
                config.setHeight((size.height * scale).round().max(1.) as usize);
                config.setMinimumFrameInterval(CMTime::new(1, FPS));
                config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
                config.setQueueDepth(3);
                config.setShowsCursor(false);
                config.setScalesToFit(true);
                config.setIgnoreShadowsSingleWindow(true);
                config.setIgnoreGlobalClipSingleWindow(true);
                config.setCaptureResolution(SCCaptureResolutionType::Best);
            }
            let output: Retained<Output> = unsafe {
                msg_send![
                    super(Output::alloc().set_ivars(OutputIvars(layer.clone()))),
                    init
                ]
            };
            let stream = unsafe {
                SCStream::initWithFilter_configuration_delegate(
                    <SCStream as AnyThread>::alloc(),
                    &filter,
                    &config,
                    None,
                )
            };
            let queue = DispatchQueue::new("rift-pip.capture", DispatchQueueAttr::SERIAL);
            if let Err(e) = unsafe {
                stream.addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&*queue),
                )
            } {
                fatal(
                    "could not attach the capture output",
                    Retained::as_ptr(&e) as *mut NSError,
                )
            }
            let started = RcBlock::new(|error: *mut NSError| {
                if !error.is_null() {
                    fatal("capture could not start", error)
                }
            });
            unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) }
            std::mem::forget((stream, output, queue));
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(true, false, &listed)
    }
}

fn corner() -> Result<Option<Corner>> {
    Ok(Some(match std::env::args().nth(1).as_deref() {
        None | Some("--bottom_right" | "--bottom-right") => Corner::BottomRight,
        Some("--top_left" | "--top-left") => Corner::TopLeft,
        Some("--top_right" | "--top-right") => Corner::TopRight,
        Some("--bottom_left" | "--bottom-left") => Corner::BottomLeft,
        Some("-h" | "--help") => {
            println!(
                "rift-pip {}\n\nUSAGE:\n    Focus a Rift-managed window, then run: rift-pip [CORNER]\n\nCORNER (default: --bottom_right):\n    --top_left | --top-left\n    --top_right | --top-right\n    --bottom_left | --bottom-left\n    --bottom_right | --bottom-right\n\nCONTROLS:\n    Drag       Move and resize the PiP while preserving its aspect ratio\n    Click      Focus the source window\n    Red button Close the PiP",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(None);
        }
        Some("-V" | "--version") => {
            println!("rift-pip {}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        }
        Some(arg) => return Err(format!("unknown option `{arg}`; try `rift-pip --help`").into()),
    }))
}

fn run() -> Result {
    let Some(corner) = corner()? else {
        return Ok(());
    };
    let mtm = MainThreadMarker::new().ok_or("must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let client = RiftMachClient::connect()
        .map_err(|e| format!("could not connect to Rift; start Rift first ({e})"))?;
    let source = client
        .get_windows(None)
        .map_err(|e| format!("could not query Rift windows ({e})"))?
        .into_iter()
        .find(|w| w.is_focused)
        .ok_or("Rift has no focused managed window; focus one and run again")?;
    let wid = source
        .window_server_id
        .ok_or("the focused Rift window has no WindowServer ID")?;
    let size = pip_size(source.frame.size.width, source.frame.size.height);
    let visible = NSScreen::mainScreen(mtm)
        .ok_or("macOS reported no main display")?
        .visibleFrame();
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            spawn(visible, size, corner),
            NSWindowStyleMask::Borderless | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setContentAspectRatio(size);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setOpaque(false);
    window.setLevel(NSFloatingWindowLevel);
    window.setHasShadow(true);
    window.setMovableByWindowBackground(true);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    unsafe {
        let _: () = msg_send![&*window, setAccessibilitySubrole: ns_string!("AXFloatingWindow")];
        let number: isize = msg_send![&*window, windowNumber];
        let mut tag = 0x0800;
        if number > 0 && SLSSetWindowTags(SLSMainConnectionID(), number as u32, &mut tag, 64) != 0 {
            eprintln!("warning: could not apply the private sticky window tag")
        }
    }
    let scale = window.backingScaleFactor();
    let layer = CALayer::layer();
    unsafe {
        let _: () = msg_send![&*layer, setContentsScale: scale];
        let _: () = msg_send![&*layer, setCornerRadius: 12.];
    }
    layer.setContentsGravity(unsafe { kCAGravityResize });
    layer.setMasksToBounds(true);
    let close = CALayer::layer();
    unsafe {
        let _: () =
            msg_send![&*close, setFrame: close_frame(rect(0., 0., size.width, size.height))];
        let _: () = msg_send![&*close, setCornerRadius: CLOSE / 2.];
        let _: () = msg_send![&*close, setBackgroundColor: &*NSColor::systemRedColor().CGColor()];
    }
    close.setAutoresizingMask(
        CAAutoresizingMask::LayerMaxXMargin | CAAutoresizingMask::LayerMinYMargin,
    );
    close.setHidden(true);
    layer.addSublayer(&close);
    let view: Retained<PipView> = unsafe {
        msg_send![super(PipView::alloc(mtm).set_ivars(PipIvars(client, source.id, wid, close))), initWithFrame: rect(0., 0., size.width, size.height)]
    };
    let tracking = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            <NSTrackingArea as AnyThread>::alloc(),
            CGRect::ZERO,
            NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect,
            Some(&*(&*view as *const PipView as *const AnyObject)),
            None,
        )
    };
    unsafe {
        let _: () = msg_send![&*view, addTrackingArea: &*tracking];
    }
    view.setLayer(Some(&layer));
    view.setWantsLayer(true);
    window.setContentView(Some(&view));
    window.orderFrontRegardless();
    let name = source.app_name.as_deref().unwrap_or("window");
    let title = if source.title.is_empty() {
        String::new()
    } else {
        format!(" — {}", source.title)
    };
    eprintln!("mirroring {name}{title}; drag to move, click to focus, hover top-left to close");
    capture(wid, layer, size, scale);
    let _window = window;
    app.run();
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        process::exit(1)
    }
}
