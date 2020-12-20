use futures_util::future::select;
use futures_util::pin_mut;
use json::JsonValue;
use linked_hash_map::LinkedHashMap;
use log::{info,warn,error};
use std::cell::{Cell,RefCell};
use std::collections::HashMap;
use std::error::Error;
use std::time::Instant;
use std::rc::Rc;
use wayland_client::Attached;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::wlr::unstable::layer_shell::v1::client as layer_shell;

use layer_shell::zwlr_layer_shell_v1::{ZwlrLayerShellV1, Layer};
use layer_shell::zwlr_layer_surface_v1::{ZwlrLayerSurfaceV1, Anchor};

use crate::item::*;
use crate::data::Variable;
use crate::wayland::WaylandClient;

/// A single taskbar on a single output
pub struct Bar {
    pub surf : Attached<WlSurface>,
    pub ls_surf : Attached<ZwlrLayerSurfaceV1>,
    pub sink : EventSink,
    width : i32,
    height : i32,
    dirty : bool,
    item : Item,
}

impl Bar {
    fn render(&mut self, surf : &cairo::ImageSurface, runtime : &Runtime) {
        let ctx = cairo::Context::new(surf);
        let font = pango::FontDescription::new();
        ctx.set_operator(cairo::Operator::Clear);
        ctx.paint();
        ctx.set_operator(cairo::Operator::Over);
        ctx.move_to(0.0, 0.0);

        let ctx = Render {
            cairo : &ctx,
            font : &font,
            align : Align::bar_default(),
            runtime,
        };

        self.sink = self.item.render(&ctx);
    }
}

/// Common state available during rendering operations
pub struct Runtime {
    pub vars : LinkedHashMap<String, Variable>,
    pub items : HashMap<String, Item>,
    pub notify : Rc<tokio::sync::Notify>,
    refresh : Rc<RefreshState>,
}

#[derive(Default)]
struct RefreshState {
    time : Cell<Option<Instant>>,
    notify : tokio::sync::Notify,
}

impl Runtime {
    pub fn set_wake_at(&self, wake : Instant) {
        match self.refresh.time.get() {
            Some(t) if t < wake => return,
            _ => ()
        }

        self.refresh.time.set(Some(wake));
        self.refresh.notify.notify_one();
    }

    pub fn format(&self, fmt : &str) -> Result<String, strfmt::FmtError> {
        strfmt::strfmt_map(fmt, &|mut q| {
            let (name, key) = match q.key.find('.') {
                Some(p) => (&q.key[..p], &q.key[p + 1..]),
                None => (&q.key[..], ""),
            };
            match self.vars.get(name) {
                Some(var) => {
                    var.read_in(name, key, self, |s| q.str(s))
                }
                None => Err(strfmt::FmtError::KeyError(name.to_string()))
            }
        })
    }

    pub fn format_or(&self, fmt : &str, context : &str) -> String {
        match self.format(fmt) {
            Ok(v) => v,
            Err(e) => {
                warn!("Error formatting '{}': {}", context, e);
                String::new()
            }
        }
    }
}

/// The singleton global state object bound to the calloop runner
pub struct State {
    pub wayland : WaylandClient,
    pub bars : Vec<Bar>,
    config : JsonValue,
    pub runtime : Runtime,
}

impl State {
    pub fn new(wayland : WaylandClient) -> Result<Rc<RefCell<Self>>, Box<dyn Error>> {
        let cfg = std::fs::read_to_string("rwaybar.json")?;
        let config = json::parse(&cfg)?;

        let items = config["items"].entries().map(|(key, value)| {
            let key = key.to_owned();
            let value = Item::from_json_txt(value);
            (key, value)
        }).collect();

        let vars = config["vars"].entries().map(Variable::new).collect();

        let mut state = Self {
            wayland,
            bars : Vec::new(),
            runtime : Runtime {
                vars,
                items,
                refresh : Default::default(),
                notify : Rc::new(tokio::sync::Notify::new()),
            },
            config,
        };

        state.runtime.vars.insert("item".into(), Variable::new_current_item());

        for (k,v) in &state.runtime.vars {
            v.init(k, &state.runtime);
        }
        state.set_data();

        let notify = state.runtime.notify.clone();
        let refresh = state.runtime.refresh.clone();
        tokio::task::spawn_local(async move {
            loop {
                use futures_util::future::Either;
                if let Some(deadline) = refresh.time.get() {
                    let sleep = tokio::time::sleep_until(deadline.into());
                    let wake = refresh.notify.notified();
                    pin_mut!(sleep, wake);
                    match select(sleep, wake).await {
                        Either::Left(_) => {
                            log::debug!("wake_at triggered refresh");
                            refresh.time.set(None);
                            notify.notify_one();
                        }
                        _ => {}
                    }
                } else {
                    refresh.notify.notified().await;
                }
            }
        });

        let notify = state.runtime.notify.clone();
        let rv = Rc::new(RefCell::new(state));
        let state = rv.clone();
        tokio::task::spawn_local(async move {
            loop {
                notify.notified().await;
                state.borrow_mut().tick();
            }
        });


        Ok(rv)
    }

    pub fn request_draw(&mut self) {
        self.runtime.notify.notify_one();
    }

    fn draw_now(&mut self) -> Result<(), Box<dyn Error>> {
        let mut shm_size = 0;
        let shm_pos : Vec<_> = self.bars.iter().map(|bar| {
            let pos = shm_size;
            let len = if bar.dirty {
                let stride = cairo::Format::ARgb32.stride_for_width(bar.width as u32).unwrap();
                (bar.height as usize) * (stride as usize)
            } else {
                0
            };
            shm_size += len;
            (pos, len)
        }).collect();

        if shm_size == 0 {
            return Ok(());
        }

        self.wayland.shm.resize(shm_size).expect("OOM");

        for (bar, (pos, len)) in self.bars.iter_mut().zip(shm_pos) {
            if !bar.dirty {
                continue;
            }
            let stride = cairo::Format::ARgb32.stride_for_width(bar.width as u32).unwrap();
            let buf : &mut [u8] = &mut self.wayland.shm.mmap().as_mut()[pos..][..len];
            unsafe {
                // cairo::ImageSurface::create_for_data requires a 'static type, so give it that
                // (this could be done safely by using a wrapper object and mem::replace on the shm object)
                let buf : &'static mut [u8] = &mut *(buf as *mut [u8]);
                let surf = cairo::ImageSurface::create_for_data(buf, cairo::Format::ARgb32, bar.width, bar.height, stride)?;
                // safety: ImageSurface never gives out direct access to D
                bar.render(&surf, &self.runtime);
                // safety: we must finish the cairo surface to end the 'static borrow
                surf.finish();
                drop(surf);
            }
            let buf = self.wayland.shm.buffer(pos as i32, bar.width, bar.height, stride, smithay_client_toolkit::shm::Format::Argb8888);
            bar.surf.attach(Some(&buf), 0, 0);
            bar.surf.damage_buffer(0, 0, bar.width, bar.height);
            bar.surf.commit();
            bar.dirty = false;
        }
        self.wayland.display.flush()?;
        Ok(())
    }

    pub fn output_ready(&mut self, i : usize) {
        let data = &self.wayland.outputs[i];
        info!("Output[{}] name='{}' description='{}' at {},{} {}x{}",
            i, data.name, data.description, data.pos_x, data.pos_y, data.size_x, data.size_y);
        for cfg in self.config["bars"].members() {
            if let Some(name) = cfg["name"].as_str() {
                if name != &data.name {
                    continue;
                }
            }

            let bar = self.new_bar(&data.output, cfg);
            self.bars.push(bar);
        }
    }

    fn new_bar(&self, output : &WlOutput, cfg : &JsonValue) -> Bar {
        let ls : Attached<ZwlrLayerShellV1> = self.wayland.env.require_global();
        let surf : Attached<_> = self.wayland.env.create_surface();
        let ls_surf = ls.get_layer_surface(&surf, Some(output), Layer::Top, "bar".to_owned());

        let size = cfg["size"].as_u32().unwrap_or(20);

        match cfg["side"].as_str() {
            Some("top") => {
                ls_surf.set_size(0, size);
                ls_surf.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
            }
            None | Some("bottom") => {
                ls_surf.set_size(0, size);
                ls_surf.set_anchor(Anchor::Bottom | Anchor::Left | Anchor::Right);
            }
            Some(side) => {
                error!("Unknown side '{}', defaulting to bottom", side);
                ls_surf.set_size(0, size);
                ls_surf.set_anchor(Anchor::Bottom | Anchor::Left | Anchor::Right);
            }
        }
        ls_surf.set_exclusive_zone(size as i32);
        ls_surf.quick_assign(move |ls_surf, event, mut data| {
            use layer_shell::zwlr_layer_surface_v1::Event;
            let state : &mut State = data.get().unwrap();
            match event {
                Event::Configure { serial, width, height } => {
                    for bar in &mut state.bars {
                        if bar.ls_surf != *ls_surf {
                            continue;
                        }

                        bar.width = width as i32;
                        bar.height = height as i32;

                        ls_surf.ack_configure(serial);
                        bar.dirty = true;
                    }
                    state.request_draw();
                }
                Event::Closed => {
                    state.bars.retain(|bar| {
                        if bar.ls_surf == *ls_surf {
                            bar.ls_surf.destroy();
                            bar.surf.destroy();
                            false
                        } else {
                            true
                        }
                    });
                },
                _ => ()
            }
        });

        surf.commit();

        Bar {
            surf,
            ls_surf : ls_surf.into(),
            item : Item::new_bar(cfg),
            width : 0,
            height : 0,
            sink : EventSink::default(),
            dirty : false,
        }
    }

    fn set_data(&mut self) {
        for (k, v) in &self.runtime.vars {
            v.update(k, &self.runtime);
        }

        // TODO maybe don't refresh all bars all the time?  Needs real dirty tracking.
        for bar in &mut self.bars {
            bar.dirty = true;
        }
    }

    pub fn tick(&mut self) {
        self.set_data();
        self.draw_now().expect("Render error");
    }
}
