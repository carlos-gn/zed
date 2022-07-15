mod events;

use alacritty_terminal::{
    ansi::{ClearMode, Handler},
    config::{Config, PtyConfig},
    event::{Event as AlacTermEvent, Notify},
    event_loop::{EventLoop, Msg, Notifier},
    grid::Scroll,
    sync::FairMutex,
    term::SizeInfo,
    tty::{self, setup_env},
    Term,
};
use futures::{channel::mpsc::unbounded, StreamExt};
use settings::Settings;
use std::{collections::HashMap, path::PathBuf, sync::Arc};

use gpui::{keymap::Keystroke, ClipboardItem, CursorStyle, Entity, ModelContext};

use crate::{
    color_translation::{get_color_at_index, to_alac_rgb},
    ZedListener,
};

use self::events::to_esc_str;

const DEFAULT_TITLE: &str = "Terminal";

///Upward flowing events, for changing the title and such
#[derive(Copy, Clone, Debug)]
pub enum Event {
    TitleChanged,
    CloseTerminal,
    Activate,
    Wakeup,
    Bell,
}

pub struct TerminalConnection {
    pub pty_tx: Notifier,
    pub term: Arc<FairMutex<Term<ZedListener>>>,
    pub title: String,
    pub associated_directory: Option<PathBuf>,
}

impl TerminalConnection {
    pub fn new(
        working_directory: Option<PathBuf>,
        initial_size: SizeInfo,
        cx: &mut ModelContext<Self>,
    ) -> TerminalConnection {
        let pty_config = PtyConfig {
            shell: None, //Use the users default shell
            working_directory: working_directory.clone(),
            hold: false,
        };

        let mut env: HashMap<String, String> = HashMap::new();
        //TODO: Properly set the current locale,
        env.insert("LC_ALL".to_string(), "en_US.UTF-8".to_string());

        let config = Config {
            pty_config: pty_config.clone(),
            env,
            ..Default::default()
        };

        setup_env(&config);

        //Spawn a task so the Alacritty EventLoop can communicate with us in a view context
        let (events_tx, mut events_rx) = unbounded();

        //Set up the terminal...
        let term = Term::new(&config, initial_size, ZedListener(events_tx.clone()));
        let term = Arc::new(FairMutex::new(term));

        //Setup the pty...
        let pty = tty::new(&pty_config, &initial_size, None).expect("Could not create tty");

        //And connect them together
        let event_loop = EventLoop::new(
            term.clone(),
            ZedListener(events_tx.clone()),
            pty,
            pty_config.hold,
            false,
        );

        //Kick things off
        let pty_tx = event_loop.channel();
        let _io_thread = event_loop.spawn();

        cx.spawn_weak(|this, mut cx| async move {
            //Listen for terminal events
            while let Some(event) = events_rx.next().await {
                match this.upgrade(&cx) {
                    Some(this) => {
                        this.update(&mut cx, |this, cx| {
                            this.process_terminal_event(event, cx);
                            cx.notify();
                        });
                    }
                    None => break,
                }
            }
        })
        .detach();

        TerminalConnection {
            pty_tx: Notifier(pty_tx),
            term,
            title: DEFAULT_TITLE.to_string(),
            associated_directory: working_directory,
        }
    }

    ///Takes events from Alacritty and translates them to behavior on this view
    fn process_terminal_event(
        &mut self,
        event: alacritty_terminal::event::Event,
        cx: &mut ModelContext<Self>,
    ) {
        match event {
            // TODO: Handle is_self_focused in subscription on terminal view
            AlacTermEvent::Wakeup => {
                cx.emit(Event::Wakeup);
            }
            AlacTermEvent::PtyWrite(out) => self.write_to_pty(out),
            AlacTermEvent::MouseCursorDirty => {
                //Calculate new cursor style.
                //TODO: alacritty/src/input.rs:L922-L939
                //Check on correctly handling mouse events for terminals
                cx.platform().set_cursor_style(CursorStyle::Arrow); //???
            }
            AlacTermEvent::Title(title) => {
                self.title = title;
                cx.emit(Event::TitleChanged);
            }
            AlacTermEvent::ResetTitle => {
                self.title = DEFAULT_TITLE.to_string();
                cx.emit(Event::TitleChanged);
            }
            AlacTermEvent::ClipboardStore(_, data) => {
                cx.write_to_clipboard(ClipboardItem::new(data))
            }
            AlacTermEvent::ClipboardLoad(_, format) => self.write_to_pty(format(
                &cx.read_from_clipboard()
                    .map(|ci| ci.text().to_string())
                    .unwrap_or("".to_string()),
            )),
            AlacTermEvent::ColorRequest(index, format) => {
                let color = self.term.lock().colors()[index].unwrap_or_else(|| {
                    let term_style = &cx.global::<Settings>().theme.terminal;
                    to_alac_rgb(get_color_at_index(&index, &term_style.colors))
                });
                self.write_to_pty(format(color))
            }
            AlacTermEvent::CursorBlinkingChange => {
                //TODO: Set a timer to blink the cursor on and off
            }
            AlacTermEvent::Bell => {
                cx.emit(Event::Bell);
            }
            AlacTermEvent::Exit => cx.emit(Event::CloseTerminal),
        }
    }

    ///Write the Input payload to the tty. This locks the terminal so we can scroll it.
    pub fn write_to_pty(&mut self, input: String) {
        self.write_bytes_to_pty(input.into_bytes());
    }

    ///Write the Input payload to the tty. This locks the terminal so we can scroll it.
    fn write_bytes_to_pty(&mut self, input: Vec<u8>) {
        self.term.lock().scroll_display(Scroll::Bottom);
        self.pty_tx.notify(input);
    }

    ///Resize the terminal and the PTY. This locks the terminal.
    pub fn set_size(&mut self, new_size: SizeInfo) {
        self.pty_tx.0.send(Msg::Resize(new_size)).ok();
        self.term.lock().resize(new_size);
    }

    pub fn clear(&mut self) {
        self.write_to_pty("\x0c".into());
        self.term.lock().clear_screen(ClearMode::Saved);
    }

    pub fn try_keystroke(&mut self, keystroke: &Keystroke) -> bool {
        let guard = self.term.lock();
        let mode = guard.mode();
        let esc = to_esc_str(keystroke, mode);
        dbg!(&esc);
        drop(guard);
        if esc.is_some() {
            self.write_to_pty(esc.unwrap());
            true
        } else {
            false
        }
    }
}

impl Drop for TerminalConnection {
    fn drop(&mut self) {
        self.pty_tx.0.send(Msg::Shutdown).ok();
    }
}

impl Entity for TerminalConnection {
    type Event = Event;
}
