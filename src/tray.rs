use crate::util::Cell;
use crate::dbus::get as get_dbus;
use crate::dbus as dbus_util;
use crate::data::Module;
use crate::icon;
use crate::item::{Item,Render,EventSink,PopupDesc};
use crate::state::{Runtime,NotifierList};
use dbus::arg::RefArg;
use dbus::arg::Variant;
use dbus::channel::MatchingReceiver;
use dbus::message::{MatchRule,Message};
use dbus::nonblock::Proxy;
use dbus::nonblock::stdintf::org_freedesktop_dbus::Properties;
use dbus::nonblock::stdintf::org_freedesktop_dbus::PropertiesPropertiesChanged;
use dbus::nonblock::stdintf::org_freedesktop_dbus::RequestNameReply;
use once_cell::unsync::OnceCell;
use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc;
use std::time::{SystemTime,Duration,UNIX_EPOCH};
use log::{debug,warn};

thread_local! {
    static DATA : OnceCell<Tray> = Default::default();
}

#[derive(Debug,Default)]
struct TrayItem {
    owner : String,
    path : String,
    is_kde : bool,

    id : String,
    title : String,
    status : String,
    icon : String,
    icon_path : String,
    menu_path : String,
    menu : Rc<Cell<Option<TrayPopupMenu>>>,
}

#[derive(Debug,Default)]
struct Tray {
    items : Cell<Vec<TrayItem>>,
    interested : Cell<NotifierList>,
}

fn init() -> Tray {
    for &(is_kde, who) in &[(true, "kde"), (false, "freedesktop")] {
        tokio::task::spawn_local(async move {
            let snw_path = format!("org.{}.StatusNotifierWatcher", who);
            let dbus = get_dbus();
            let name = format!("org.{}.StatusNotifierHost-{}", who, std::process::id());
            match dbus.local.request_name(&name, false, false, true).await {
                Ok(RequestNameReply::PrimaryOwner) => {}
                _ => {
                    warn!("Could not register as tray ({})", who);
                    return Ok(());
                }
            }

            // TODO actually implement StatusNotifierWatcher ourselves
            if false {
                match dbus.local.request_name(&snw_path, true, false, false).await {
                    Ok(_) => {}
                    _ => {
                        warn!("Could not register as StatusNotifierWatcher, tray may not work ({})", who);
                    }
                }
            }

            let prop_rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
            dbus.local.start_receive(prop_rule, Box::new(move |msg, _local| {
                handle_item_update_msg(msg);
                true
            }));

            let mut item_rule = MatchRule::new_signal(snw_path.clone(), "x");
            item_rule.member = None;
            dbus.local.add_match_no_cb(&item_rule.match_str()).await?;

            dbus.local.start_receive(item_rule, Box::new(move |msg : Message, _local| {
                let item : String = match msg.get1() {
                    Some(s) => s,
                    None => return true,
                };
                match msg.member().as_deref().unwrap_or("") {
                    "StatusNotifierItemRegistered" => do_add_item(is_kde, item),
                    "StatusNotifierItemUnregistered" => do_del_item(item),
                    _ => ()
                }
                true
            }));

            let watcher = Proxy::new(&snw_path, "/StatusNotifierWatcher", Duration::from_secs(10), &dbus.local);
            dbg!(watcher.method_call(&snw_path, "RegisterStatusNotifierHost", (&name,)).await)?;

            let items : Vec<String> = watcher.get(&snw_path, "RegisteredStatusNotifierItems").await?;

            for item in items {
                do_add_item(is_kde, item);
            }

            Ok::<(), Box<dyn Error>>(())
        });
    }


    Tray::default()
}

fn do_add_item(is_kde : bool, item : String) {
    let sni_path = if is_kde { "org.kde.StatusNotifierItem" } else { "org.freedesktop.StatusNotifierItem" };
    tokio::task::spawn_local(async move {
        let dbus = get_dbus();

        let (owner, path) = match item.find('/') {
            Some(pos) => (&item[..pos], &item[pos..]),
            None => return Ok(()),
        };

        let mut notify_rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
        notify_rule.path = Some(path.into());
        notify_rule.sender = Some(owner.into());
        dbus.local.add_match_no_cb(&notify_rule.match_str()).await?;

        let mut item = TrayItem::default();
        item.owner = owner.into();
        item.path = path.into();
        item.is_kde = is_kde;
        DATA.with(|cell| {
            let tray = cell.get();
            let tray = tray.as_ref().unwrap();
            tray.items.take_in(|items| {
                items.push(item);
            });
            tray.interested.take().notify_data();
        });

        let proxy = Proxy::new(owner, path, Duration::from_secs(10), &dbus.local);
        let props = proxy.get_all(&sni_path).await?;

        handle_item_update(owner, path, &props);
        Ok::<(), Box<dyn Error>>(())
    });
}

fn do_del_item(item : String) {
    let (owner, path) = match item.find('/') {
        Some(pos) => (&item[..pos], &item[pos..]),
        None => return,
    };

    DATA.with(|cell| {
        let tray = cell.get();
        let tray = tray.as_ref().unwrap();
        tray.items.take_in(|items| {
            items.retain(|item| item.owner != owner || item.path != path);
        });
        tray.interested.take().notify_data();
    });
}

fn handle_item_update_msg(msg : Message) {
    let src = msg.sender().unwrap();
    let path = msg.path().unwrap();
    let p = match msg.read_all::<PropertiesPropertiesChanged>() {
        Ok(p) if p.interface_name == "org.kde.StatusNotifierItem" => p,
        Ok(p) if p.interface_name == "org.freedesktop.StatusNotifierItem" => p,
        _ => return,
    };
    handle_item_update(&src, &path, &p.changed_properties);
}

fn handle_item_update(owner : &str, path : &str, props : &HashMap<String, Variant<Box<dyn RefArg + 'static>>>) {
    DATA.with(|cell| {
        let tray = cell.get();
        let tray = tray.as_ref().unwrap();
        tray.items.take_in(|items| {
            for item in items {
                if item.owner != owner || item.path != path {
                    continue;
                }

                for (key, value) in props {
                    match key.as_str() {
                        "Id" => value.as_str().map(|v| item.id = v.into()),
                        "Title" => value.as_str().map(|v| item.title = v.into()),
                        "IconName" => value.as_str().map(|v| item.icon = v.into()),
                        "IconThemePath" => value.as_str().map(|v| item.icon_path = v.into()),
                        "Menu" => value.as_str().map(|v| item.menu_path = v.into()),
                        _ => None
                    };
                }
            }
        });
        tray.interested.take().notify_data();
    });
}

#[derive(Clone,Debug)]
pub struct TrayPopup {
    owner : String,
    menu_path : String,
    title : String,
    menu : Rc<Cell<Option<TrayPopupMenu>>>,
    rendered_ids : Vec<(f64, f64, i32)>,
}

#[derive(Debug,Default)]
struct TrayPopupMenu {
    items : Vec<MenuItem>,
    interested : NotifierList,
}

#[derive(Debug,Default)]
struct MenuItem {
    id : i32,
    is_sep : bool,
    label : String,
}

impl TrayPopup {
    pub fn get_size(&self) -> (i32, i32) {
        let tmp = cairo::RecordingSurface::create(cairo::Content::ColorAlpha, None).unwrap();
        let ctx = cairo::Context::new(&tmp);
        let layout = pangocairo::create_layout(&ctx).unwrap();
        layout.set_text(&self.title);
        let psize = layout.get_size();
        let mut size = (pango::units_to_double(psize.0), pango::units_to_double(psize.1));
        self.menu.take_in_some(|menu| {
            if !menu.items.is_empty() {
                size.1 += 9.0;
            }
            for item in &menu.items {
                if item.is_sep {
                    size.1 += 9.0;
                } else {
                    let layout = pangocairo::create_layout(&ctx).unwrap();
                    layout.set_text(&item.label);
                    let tsize = layout.get_size();
                    size.0 = f64::max(size.0, pango::units_to_double(tsize.0));
                    size.1 += pango::units_to_double(tsize.1) + 5.0;
                }
            }
        }).unwrap_or_else(|| {
            let menu = self.menu.clone();
            let owner = self.owner.clone();
            let menu_path = self.menu_path.clone();
            menu.set(Some(TrayPopupMenu::default()));
            tokio::task::spawn_local(async move {
                let dbus = get_dbus();
                let proxy = Proxy::new(owner, menu_path, Duration::from_secs(10), &dbus.local);
                let _ : (bool,) = proxy.method_call("com.canonical.dbusmenu", "AboutToShow", (0i32,)).await?;

                let (_rev, (_id, _props, contents)) : (u32, (i32, HashMap<String, Variant<Box<dyn RefArg>>>, Vec<Variant<Box<dyn RefArg>>>))
                    = proxy.method_call("com.canonical.dbusmenu", "GetLayout", (0i32, -1i32, &["type", "label"] as &[&str])).await?;

                menu.take_in_some(|menu| {
                    for Variant(v) in contents {
                        let mut item = MenuItem::default();
                        let iter = match v.as_iter() { Some(i) => i, None => continue };
                        for (i, value) in iter.enumerate() {
                            match i {
                                0 => { value.as_i64().map(|id| item.id = id as i32); }
                                1 => {
                                    let props = dbus_util::read_hash_map(&value);
                                    let props = match props { Some(i) => i, None => continue };
                                    props.get("label").and_then(|v| v.as_str())
                                        .map(|label| item.label = label.to_owned());
                                    props.get("type").and_then(|v| v.as_str())
                                        .map(|v| match v {
                                            "separator" => item.is_sep = true,
                                            _ => {}
                                        });
                                }
                                _ => break,
                            }
                        }
                        menu.items.push(item);
                    }
                    menu.interested.notify_data();
                });

                Ok::<(), Box<dyn Error>>(())
            });
        });
        (size.0 as i32 + 4, size.1 as i32 + 4)
    }

    pub fn render(&mut self, ctx : &cairo::Context, runtime : &Runtime) -> (i32, i32) {
        let clip = ctx.clip_extents(); 
        ctx.move_to(2.0, 2.0);
        let layout = pangocairo::create_layout(&ctx).unwrap();
        layout.set_text(&self.title);
        let psize = layout.get_size();
        pangocairo::show_layout(&ctx, &layout);
        let mut pos = 2.0 + pango::units_to_double(psize.1);
        let rendered_ids = &mut self.rendered_ids;
        self.menu.take_in_some(|menu| {
            menu.interested.add(runtime);
            if !menu.items.is_empty() {
                ctx.move_to(0.0, pos + 4.0);
                ctx.line_to(clip.2, pos + 4.0);
                ctx.stroke();
                pos += 9.0;
            }
            for item in &menu.items {
                if item.is_sep {
                    ctx.move_to(5.0, pos + 4.0);
                    ctx.line_to(clip.2 - 5.0, pos + 4.0);
                    ctx.stroke();
                    pos += 9.0;
                } else {
                    ctx.move_to(2.0, pos);
                    let layout = pangocairo::create_layout(&ctx).unwrap();
                    layout.set_text(&item.label);
                    let tsize = layout.get_size();
                    pangocairo::show_layout(&ctx, &layout);
                    let end = pos + pango::units_to_double(tsize.1);
                    rendered_ids.push((pos, end, item.id));
                    pos = end + 5.0;
                }
            }
        });
        // This is required because pango won't report render cropping due to widths being short
        self.get_size()
    }

    pub fn button(&mut self, x : f64, y : f64, button : u32, _runtime : &mut Runtime) {
        let _ = (x, button);
        for &(min, max, id) in &self.rendered_ids {
            let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
            if y < min || y > max {
                continue;
            }
            dbg!();
            let owner = self.owner.clone();
            let menu_path = self.menu_path.clone();
            tokio::task::spawn_local(async move {
                let dbus = get_dbus();
                let proxy = Proxy::new(owner, menu_path, Duration::from_secs(10), &dbus.local);
                dbg!(proxy.method_call("com.canonical.dbusmenu", "Event", (id, "clicked", Variant(0i32), ts as u32)).await)?;
                Ok::<(), Box<dyn Error>>(())
            });
        }
    }
}

pub fn show(ctx : &Render, ev : &mut EventSink, spacing : f64) {
    DATA.with(|cell| {
        let tray = cell.get_or_init(init);
        tray.interested.take_in(|interest| interest.add(&ctx.runtime));
        tray.items.take_in(|items| {
            ctx.cairo.rel_move_to(spacing, 0.0);
            for item in items {
                let x0 = ctx.cairo.get_current_point().0;
                let mut done = false;
                if !done && item.icon_path != "" {
                    let icon = format!("{}/{}.svg", item.icon_path, item.icon);
                    if icon::render(ctx, &icon).is_ok() {
                        done = true;
                    }
                }
                if !done && item.icon_path != "" {
                    let icon = format!("{}/{}.png", item.icon_path, item.icon);
                    if icon::render(ctx, &icon).is_ok() {
                        done = true;
                    }
                }
                if !done && icon::render(ctx, &item.icon).is_ok() {
                    done = true;
                }
                if !done {
                    let item : Item = Module::Value { value : Cell::new(item.title.clone()) }.into();
                    item.render(ctx);
                }
                let x1 = ctx.cairo.get_current_point().0;
                let mut es = EventSink::from_tray(item.owner.clone(), item.path.clone());
                es.offset_clamp(0.0, x0, x1);
                es.add_hover(x0, x1, PopupDesc::Tray(TrayPopup {
                    owner : item.owner.clone(),
                    title : item.title.clone(),
                    menu_path : item.menu_path.clone(),
                    menu : item.menu.clone(),
                    rendered_ids : Vec::new(),
                }));
                ev.merge(es);
                ctx.cairo.rel_move_to(spacing, 0.0);
            }
        });
    });
}

pub fn do_click(owner : &str, path : &str, how : u32) {
    let method = match how {
        0 => "Activate",
        1 => "ContextMenu",
        2 => "SecondaryActivate",
        5 | 6 => "vertical",
        7 | 8 => "horizontal",
        _ => return,
    };
    let owner = owner.to_owned();
    let path = path.to_owned();
    DATA.with(|cell| {
        let tray = cell.get();
        let tray = tray.as_ref().unwrap();
        tray.items.take_in(|items| {
            for item in items {
                if item.owner != owner || item.path != path {
                    continue;
                }
                let sni_path = if item.is_kde { "org.kde.StatusNotifierItem" } else { "org.freedesktop.StatusNotifierItem" };
                let id = item.id.clone();
                tokio::task::spawn_local(async move {
                    let dbus = get_dbus();
                    let proxy = Proxy::new(&owner, &path, Duration::from_secs(10), &dbus.local);
                    debug!("Invoking {} on {}", method, id);
                    if how < 3 {
                        proxy.method_call(sni_path, method, (0i32,0i32)).await?;
                    } else {
                        proxy.method_call(sni_path, "Scroll", (15i32, method)).await?;
                    }
                    Ok::<(), Box<dyn Error>>(())
                });
                return;
            }
        });
    });
}
