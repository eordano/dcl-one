use anyhow::{anyhow, bail, ensure};
use bevy::{
    asset::AssetLoader,
    ecs::{
        change_detection::MaybeLocation,
        component::Tick,
        reflect::ReflectCommandExt,
        system::{EntityCommands, SystemParam},
        world::CommandQueue,
    },
    log::{debug, error, warn},
    platform::collections::HashMap,
    prelude::*,
    tasks::ConditionalSendFuture,
    ui::FocusPolicy,
};
use bevy_ecss::{property::impls::FontColorProperty, Property, PropertyValues, Selector};
use std::{
    any::{type_name, Any, TypeId},
    collections::BTreeMap,
    marker::PhantomData,
};

/// pass properties into templates
#[derive(Default)]
pub struct DuiProps {
    props: HashMap<String, Box<dyn Any>>,
}

impl std::fmt::Debug for DuiProps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuiProps")
            .field("props", &self.props)
            .finish()
    }
}

impl AsMut<DuiProps> for DuiProps {
    fn as_mut(&mut self) -> &mut DuiProps {
        self
    }
}

impl DuiProps {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_prop<T: 'static>(mut self, key: impl Into<String>, value: T) -> Self {
        self.insert_prop(key, value);
        self
    }

    pub fn insert_prop<T: 'static>(&mut self, key: impl Into<String>, value: T) {
        self.props.insert(key.into(), Box::new(value));
    }

    /// returns Ok(Some(&value)) if the key is found and the value is of the correct type.
    /// borrow can be used to borrow default/global properties as well as non-default properties.
    ///
    /// returns Err(e) if the key is found but the value is not of the expected type.
    ///
    /// returns Ok(None) if the key is not found.
    pub fn borrow<'a, T: 'static>(
        &'a self,
        key: &str,
        ctx: &'a DuiContext,
    ) -> Result<Option<&'a T>, anyhow::Error> {
        if let Some(local_prop) = self.props.get(key) {
            Some(
                local_prop
                    .downcast_ref()
                    .ok_or_else(|| anyhow!("property {key} is not of type {}", type_name::<T>())),
            )
            .transpose()
        } else {
            ctx.registry()
                .default_props
                .get(key)
                .map(|b| {
                    b.downcast_ref().ok_or_else(|| {
                        anyhow!("property {key} is not of type {}", type_name::<T>())
                    })
                })
                .transpose()
        }
    }

    /// returns Ok(Some(value)) if the key is found and the value is of the correct type.
    /// take can only be used to take non-default/global properties.
    ///
    /// returns Err(e) if the key is found but the value is not of the expected type.
    ///
    /// returns Ok(None) if the key is not found.
    pub fn take<T: 'static>(&mut self, key: &str) -> Result<Option<T>, anyhow::Error> {
        self.props
            .remove_entry(key)
            .map(|(k, b)| {
                b.downcast().map(|b| *b).map_err(|b| {
                    self.props.insert(k, b);
                    anyhow!("property {key} is not of type {}", type_name::<T>())
                })
            })
            .transpose()
    }

    fn insert_prop_untyped(&mut self, key: impl Into<String>, value: Box<dyn Any>) {
        self.props.insert(key.into(), value);
    }

    fn take_untyped(&mut self, key: &str) -> Option<Box<dyn Any>> {
        self.props.remove(key)
    }

    fn borrow_untyped(&self, key: &str, ctx: &DuiContext) -> Option<&Box<dyn Any>> {
        if let Some(val) = self.props.get(key) {
            Some(val)
        } else {
            // SAFETY: i can't find anything that says this is really safe
            // normally you can safely coerce Box<dyn Any + Send> to Box<dyn Any>
            // but here we have &Box<dyn Any + Send> and want &Box<dyn Any>
            // maybe i could return an enum? or just not use this func and use the typed api
            ctx.registry()
                .default_props
                .get(key)
                .map(|val| unsafe { std::mem::transmute(val) })
        }
    }
}

pub type NodeMap = HashMap<String, Entity>;

/// root entity node and named nodes, returned from apply_template / spawn_template calls
#[derive(Component, Clone)]
pub struct DuiEntities {
    pub root: Entity,
    pub named_nodes: NodeMap,
}

impl DuiEntities {
    pub fn new(root: Entity) -> Self {
        Self {
            root,
            named_nodes: Default::default(),
        }
    }

    pub fn get_named(&self, name: &str) -> Option<Entity> {
        self.named_nodes.get(name).cloned()
    }

    pub fn named(&self, name: &str) -> Entity {
        self.named_nodes
            .get(name)
            .copied()
            .unwrap_or_else(|| panic!("no key {name}"))
    }
}

/// trait for custom component implementations
pub trait DuiTemplate: Send + Sync {
    fn render(
        &self,
        commands: &mut EntityCommands,
        props: DuiProps,
        ctx: &mut DuiContext,
    ) -> Result<NodeMap, anyhow::Error>;
}

#[derive(Asset, TypePath, Clone)]
pub struct DuiNodeList(Vec<DuiNode>);

#[derive(Clone)]
struct DuiNode {
    template: String,
    nodes: Vec<DuiElt>,
}

impl DuiNode {
    fn render_inner(
        commands: &mut EntityCommands,
        iter: &mut impl Iterator<Item = DuiElt>,
        mut props: DuiProps,
        ctx: &mut DuiContext,
    ) -> Result<(NodeMap, DuiProps), anyhow::Error> {
        let DuiElt {
            id,
            template,
            properties,
            mut components,
            prop_components,
            children,
        } = iter.next().ok_or(anyhow!("unexpected end of iterator"))?;

        debug!(
            "[{:?}] render inner [{:?}] [children: {:?}]",
            commands.id(),
            template,
            ctx.children
                .iter()
                .map(|rc| format!("{}", rc.1.len()))
                .collect::<Vec<_>>()
                .join(", ")
        );

        if matches!(&template, Some(PropValue::Literal(n)) if n.as_str() == "apply-children") {
            debug!("[{:?}] applying children", commands.id());
            return Ok((ctx.apply_children(commands)?, props));
        };

        // apply any @prop style components
        let mut style_props = HashMap::new();
        for (k, v) in prop_components.iter().filter_map(|(k, v)| match k {
            PropComponent::StyleAttr(s) => Some((s, v)),
            _ => None,
        }) {
            if let Some(prop) = props.borrow::<String>(v, ctx)? {
                style_props.insert(k.to_owned(), prop.clone());
            }
        }

        if !style_props.is_empty() {
            let content = format!(
                "#inline {{{}}}",
                style_props
                    .into_iter()
                    .map(|(k, v)| format!("{k}: {v}; "))
                    .collect::<Vec<_>>()
                    .join("")
            );
            let ss = bevy_ecss::StyleSheetAsset::parse("", &content);
            let mut world = World::new();
            let ent = world.spawn_empty().id();
            for rule in ss.iter() {
                for (key, value) in rule.properties.iter() {
                    if !DuiLoader::apply_props(
                        key,
                        value,
                        &mut world,
                        ent,
                        ctx.asset_server(),
                        &mut components,
                        &mut None,
                    ) {
                        panic!("this shouldn't be possible: {key} missing");
                    } else {
                        debug!("applied {} -> {:?}", key, value);
                    }
                }
            }
        }

        for (ty, component) in components.iter() {
            match ty {
                ty if ty == &TypeId::of::<Text>() => {
                    let mut text_component = Text::default();
                    text_component.apply(component.as_ref());
                    if let Some(key) = prop_components.get(&PropComponent::Text) {
                        text_component.0 = props
                            .borrow::<String>(key, ctx)?
                            .cloned()
                            .unwrap_or_default();
                    }
                    debug!("added text_component '{:?}'", text_component);
                    commands.insert(text_component);
                }
                ty if ty == &TypeId::of::<ImageNode>() => {
                    let mut ui_component = ImageNode::default();
                    ui_component.apply(component.as_ref());
                    if let Some(key) = prop_components.get(&PropComponent::Image) {
                        if let Some(untyped) = props.borrow_untyped(key, ctx) {
                            if let Some(path) = untyped.downcast_ref::<String>() {
                                ui_component.image = ctx.asset_server().load(path);
                            } else if let Some(path) = untyped.downcast_ref::<&str>() {
                                ui_component.image = ctx.asset_server().load(path.to_owned());
                            } else if let Some(handle) = untyped.downcast_ref::<Handle<Image>>() {
                                ui_component.image = handle.clone();
                            } else {
                                warn!("prop image type not recognised, expected String, &str or Handle<Image> (`{key}`)");
                            }
                        } else {
                            warn!("prop image not found (`{key}`)");
                        }
                    }
                    if let Some(key) = prop_components.get(&PropComponent::ImageColor) {
                        if let Ok(Some(color)) = props.borrow::<Color>(key, ctx) {
                            ui_component.color = *color;
                        } else {
                            warn!("image-color property not a color");
                        }
                    }
                    debug!("added ui_image component");
                    commands.insert(ui_component);
                }
                _ => {
                    debug!(
                        "[{:?}] added reflect component of type {}",
                        commands.id(),
                        PartialReflect::get_represented_type_info(component.as_ref())
                            .unwrap()
                            .type_path()
                    );
                    commands.insert_reflect(component.reflect_clone().unwrap());
                }
            }
        }

        let mut results = match template {
            None => ctx.apply_children_inner(commands, iter, children, props),
            Some(template) => {
                let template = match &template {
                    PropValue::Literal(val) => val,
                    PropValue::Prop(key) => props
                        .borrow::<String>(key, ctx)?
                        .ok_or_else(|| anyhow!("missing property for template {key}"))?,
                }
                .clone();
                debug!("[{template}]");

                let mut component_props = DuiProps::new();
                for (k, v) in properties.iter() {
                    match v {
                        PropValue::Literal(lit) => {
                            debug!("-> prop {k} -> `{lit}`");
                            component_props.insert_prop(k.clone(), lit.clone())
                        }
                        PropValue::Prop(parent_key) => {
                            if let Some(val) = props.take_untyped(parent_key) {
                                debug!("[-> prop {k} -> @{parent_key} == `{val:?}`");
                                component_props.insert_prop_untyped(k.clone(), val);
                            } else {
                                debug!("[-> prop {k} -> @`{parent_key}` missing");
                                debug!("all props: {props:#?}");
                            }
                        }
                    }
                }

                let mut elts_required = children.len();
                let mut child_elts = Vec::default();
                while elts_required > 0 {
                    let Some(elt) = iter.next() else {
                        error!(
                            "need {} children, but none left! vec looks like: {:?}",
                            elts_required, child_elts
                        );
                        panic!();
                    };
                    elts_required += elt.children.len();
                    child_elts.push(elt);
                    elts_required -= 1;
                }

                let prev_children_count = ctx.children.len();
                ctx.children.push((child_elts, children, props));

                // let component = ctx
                //     .registry()
                //     .templates
                //     .get(&template)
                //     .ok_or(anyhow!("missing component registry for `{template}`"))?;

                debug!(
                    "[{:?} adding template children ({:?} sets)",
                    commands.id(),
                    ctx.children.len()
                );
                let mut res = ctx.render_template(commands, &template, component_props)?;
                if ctx.children.len() == prev_children_count + 1 {
                    debug!(
                        "[{:?}] automatically adding children to component",
                        commands.id()
                    );
                    res.extend(ctx.apply_children(commands)?);
                }

                let props = ctx.used_props.pop().unwrap();

                Ok((res, props))
            }
        }?;

        results.0.extend(id.map(|id| (id.clone(), commands.id())));
        Ok(results)
    }
}

impl DuiTemplate for DuiNode {
    fn render(
        &self,
        commands: &mut EntityCommands,
        props: DuiProps,
        ctx: &mut DuiContext,
    ) -> Result<NodeMap, anyhow::Error> {
        let mut iter = self.nodes.clone().into_iter();
        Self::render_inner(commands, &mut iter, props, ctx).map(|(nodes, _)| nodes)
    }
}

pub struct DuiContext<'a> {
    dui: &'a DuiRegistry,
    children: Vec<(Vec<DuiElt>, Vec<ChildType>, DuiProps)>,
    used_props: Vec<DuiProps>,
}

impl<'a> DuiContext<'a> {
    pub fn registry(&self) -> &DuiRegistry {
        self.dui
    }

    pub fn asset_server(&self) -> &AssetServer {
        self.dui.asset_server()
    }

    pub fn apply_children(
        &mut self,
        target: &mut EntityCommands,
    ) -> Result<NodeMap, anyhow::Error> {
        let Some((elts, children, props)) = self.children.pop() else {
            bail!("apply children out of context");
        };
        let mut iter = elts.into_iter();

        let (named_nodes, props) = self.apply_children_inner(target, &mut iter, children, props)?;
        self.used_props.push(props);
        Ok(named_nodes)
    }

    fn apply_children_inner(
        &mut self,
        target: &mut EntityCommands,
        iter: &mut impl Iterator<Item = DuiElt>,
        children: Vec<ChildType>,
        mut props: DuiProps,
    ) -> Result<(NodeMap, DuiProps), anyhow::Error> {
        let mut named_nodes = HashMap::default();
        let root_id = target.id();

        for child_type in &children {
            match child_type {
                ChildType::SpawnNew => {
                    let mut new_commands = target.commands();
                    let mut node = new_commands.spawn_empty();
                    debug!("[{:?}] spawned child [{:?}]", root_id, node.id());
                    let res = DuiNode::render_inner(&mut node, iter, props, self)?;
                    let id = node.id();
                    target.add_children(&[id]);
                    props = res.1;
                    named_nodes.extend(res.0);
                }
                ChildType::ApplyToParent => {
                    let res = DuiNode::render_inner(target, iter, props, self)?;
                    props = res.1;
                    named_nodes.extend(res.0);
                }
            };
        }

        Ok((named_nodes, props))
    }

    pub fn render_template(
        &mut self,
        target: &mut EntityCommands,
        template: &str,
        props: DuiProps,
    ) -> Result<NodeMap, anyhow::Error> {
        let component = self
            .dui
            .templates
            .get(template)
            .ok_or(anyhow!("missing component registry for `{template}`"))?;

        let mut res = component.render(target, props, self)?;
        res.insert("root".to_owned(), target.id());
        target.insert(DuiEntities {
            root: target.id(),
            named_nodes: res.clone(),
        });
        Ok(res)
    }

    pub fn spawn_template(
        &mut self,
        template: &str,
        parent: &mut ChildSpawnerCommands,
        props: DuiProps,
    ) -> Result<NodeMap, anyhow::Error> {
        let component = self
            .dui
            .templates
            .get(template)
            .ok_or(anyhow!("missing component registry for `{template}`"))?;

        let mut target = parent.spawn_empty();
        let mut res = component.render(&mut target, props, self)?;
        res.insert("root".to_owned(), target.id());
        Ok(res)
    }
}

/// resource storing templates, used to create instances
#[derive(Resource)]
pub struct DuiRegistry {
    templates: HashMap<String, Box<dyn DuiTemplate>>,
    default_props: HashMap<String, Box<dyn Any + Send + Sync + 'static>>,
    asset_server: AssetServer,
}

impl FromWorld for DuiRegistry {
    fn from_world(world: &mut World) -> Self {
        let asset_server = world.resource::<AssetServer>().clone();
        Self {
            templates: default(),
            default_props: Default::default(),
            asset_server,
        }
    }
}

impl DuiRegistry {
    pub fn register_template(
        &mut self,
        name: impl Into<String>,
        template: impl DuiTemplate + 'static,
    ) {
        let name = name.into();
        debug!("added template {name}");
        if self.templates.contains_key(&name) {
            warn!("overwriting template {name}");
        }
        self.templates.insert(name, Box::new(template));
    }

    pub fn apply_template(
        &self,
        commands: &mut EntityCommands,
        template: &str,
        mut props: DuiProps,
    ) -> Result<DuiEntities, anyhow::Error> {
        debug!(
            "[{:?}] applying template {template} with props: {:?}",
            commands.id(),
            props.as_mut().props.keys()
        );

        let mut ctx = DuiContext {
            dui: self,
            children: Default::default(),
            used_props: Default::default(),
        };

        let named_nodes = self
            .templates
            .get(template)
            .ok_or(anyhow!("template `{template}` not found"))?
            .render(commands, props, &mut ctx)?;

        let dui_entities = DuiEntities {
            root: commands.id(),
            named_nodes,
        };

        commands.insert(dui_entities.clone());

        Ok(dui_entities)
    }

    pub fn set_default_prop<C: Clone + Send + Sync + 'static>(
        &mut self,
        key: impl Into<String>,
        val: C,
    ) {
        self.default_props.insert(key.into(), Box::new(val));
    }

    pub fn asset_server(&self) -> &AssetServer {
        &self.asset_server
    }
}

#[derive(Clone, Debug)]
pub enum PropValue {
    Literal(String),
    Prop(String),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum PropComponent {
    Text,
    Image,
    ImageColor,
    StyleAttr(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChildType {
    SpawnNew,
    ApplyToParent,
}

#[derive(Default, Debug)]
pub struct DuiElt {
    id: Option<String>,
    template: Option<PropValue>,
    properties: HashMap<String, PropValue>,
    components: BTreeMap<TypeId, Box<dyn Reflect>>,
    prop_components: HashMap<PropComponent, String>,
    children: Vec<ChildType>,
}

impl Clone for DuiElt {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            template: self.template.clone(),
            properties: self.properties.clone(),
            components: BTreeMap::from_iter(
                self.components
                    .iter()
                    .map(|(k, v)| (*k, v.reflect_clone().unwrap())),
            ),
            prop_components: self.prop_components.clone(),
            children: self.children.clone(),
        }
    }
}

macro_rules! apply_prop {
    ($key:ident, $value:ident, $world:ident, $asset_server:ident, $components:ident, $prop_components:ident, $T:ty) => {
        if $key == <$T>::name() {
            let mut queue = CommandQueue::default();
            let mut commands = Commands::new(&mut queue, $world);
            let reflected_component = $components.entry(TypeId::of::<<<$T as Property>::Components as std::ops::Deref>::Target>()).or_insert_with(|| {
                let val = <<<$T as Property>::Components as std::ops::Deref>::Target as Default>::default();
                Box::new(val).into_reflect()
            });
            let target = if let Some(target) = reflected_component.as_any_mut().downcast_mut() {
                target
            } else {
                let mut new_value = <<<$T as Property>::Components as std::ops::Deref>::Target as Default>::default();
                new_value.apply(reflected_component.as_ref());
                *reflected_component = Box::new(new_value).into_reflect();
                reflected_component.as_any_mut().downcast_mut().unwrap()
            };
            let mut ta = Tick::new(0);
            let mut tb = Tick::new(0);
            let mut caller = MaybeLocation::caller();
            let mut_wrapper = Mut::new(target, &mut ta, &mut tb, Tick::new(0), Tick::new(0), caller.as_mut());
            if matches!($value.first(), Some(bevy_ecss::PropertyToken::String(s)) if s.starts_with('@')) {
                let bevy_ecss::PropertyToken::String(s)= $value.first().unwrap() else { panic!() };
                $prop_components.as_mut().unwrap().insert(PropComponent::StyleAttr($key.to_owned()), s[1..].to_owned());
            } else if let Ok(val) = <$T as Property>::parse($value) {
                <$T>::apply(&val, mut_wrapper, $asset_server, &mut commands);
            } else {
                warn!("failed to parse {} property value {:?}", std::any::type_name::<$T>(), $value);
            };
            true
        } else {
            false
        }
    }
}

macro_rules! apply_entity_prop {
    ($key:ident, $value:ident, $world:ident, $asset_server:ident, $entity:ident, $components:ident, $prop_components:ident, $T:ty, $C:ty) => {
        if $key == <$T>::name() {
            let mut queue = CommandQueue::default();
            let mut commands = Commands::new(&mut queue, $world);
            if matches!($value.first(), Some(bevy_ecss::PropertyToken::String(s)) if s.starts_with('@')) {
                let bevy_ecss::PropertyToken::String(s)= $value.first().unwrap() else { panic!() };
                $prop_components.as_mut().unwrap().insert(PropComponent::StyleAttr($key.to_owned()), s[1..].to_owned());
                $components.insert(
                    TypeId::of::<$C>(),
                    Box::<$C>::default().into_reflect(),
                );
            } else if let Ok(val) = <$T as Property>::parse($value) {
                <$T>::apply(&val, $entity, $asset_server, &mut commands);
                queue.apply($world);
                let component = $world.get::<$C>($entity).unwrap();
                $components.insert(
                    TypeId::of::<$C>(),
                    Box::new(component.clone()).into_reflect(),
                );
            } else {
                warn!(
                    "failed to parse {} property value {:?}",
                    std::any::type_name::<$T>(),
                    $value
                );
            };
            true
        } else {
            false
        }
    };
}

impl DuiLoader {
    fn apply_props(
        k: &str,
        v: &PropertyValues,
        w: &mut World,
        e: Entity,
        a: &AssetServer,
        c: &mut BTreeMap<TypeId, Box<dyn Reflect>>,
        p: &mut Option<&mut HashMap<PropComponent, String>>,
    ) -> bool {
        use bevy_ecss::property::impls::*;

        apply_prop!(k, v, w, a, c, p, DisplayProperty)
            || apply_prop!(k, v, w, a, c, p, PositionTypeProperty)
            || apply_prop!(k, v, w, a, c, p, FlexDirectionProperty)
            || apply_prop!(k, v, w, a, c, p, FlexWrapProperty)
            || apply_prop!(k, v, w, a, c, p, AlignItemsProperty)
            || apply_prop!(k, v, w, a, c, p, AlignSelfProperty)
            || apply_prop!(k, v, w, a, c, p, AlignContentProperty)
            || apply_prop!(k, v, w, a, c, p, JustifyContentProperty)
            || apply_prop!(k, v, w, a, c, p, OverflowAxisXProperty)
            || apply_prop!(k, v, w, a, c, p, OverflowAxisYProperty)
            || apply_prop!(k, v, w, a, c, p, LeftProperty)
            || apply_prop!(k, v, w, a, c, p, RightProperty)
            || apply_prop!(k, v, w, a, c, p, TopProperty)
            || apply_prop!(k, v, w, a, c, p, BottomProperty)
            || apply_prop!(k, v, w, a, c, p, WidthProperty)
            || apply_prop!(k, v, w, a, c, p, HeightProperty)
            || apply_prop!(k, v, w, a, c, p, MinWidthProperty)
            || apply_prop!(k, v, w, a, c, p, MinHeightProperty)
            || apply_prop!(k, v, w, a, c, p, MaxWidthProperty)
            || apply_prop!(k, v, w, a, c, p, MaxHeightProperty)
            || apply_prop!(k, v, w, a, c, p, FlexBasisProperty)
            || apply_prop!(k, v, w, a, c, p, FlexGrowProperty)
            || apply_prop!(k, v, w, a, c, p, FlexShrinkProperty)
            || apply_prop!(k, v, w, a, c, p, AspectRatioProperty)
            || apply_prop!(k, v, w, a, c, p, MarginProperty)
            || apply_prop!(k, v, w, a, c, p, PaddingProperty)
            || apply_prop!(k, v, w, a, c, p, BorderProperty)
            || apply_prop!(k, v, w, a, c, p, FontColorProperty)
            || apply_prop!(k, v, w, a, c, p, FontProperty)
            || apply_prop!(k, v, w, a, c, p, FontSizeProperty)
            || apply_prop!(k, v, w, a, c, p, TextAlignProperty)
            || apply_prop!(k, v, w, a, c, p, TextContentProperty)
            || apply_prop!(k, v, w, a, c, p, ImageProperty)
            || apply_entity_prop!(
                k,
                v,
                w,
                a,
                e,
                c,
                p,
                BackgroundColorProperty,
                BackgroundColor
            )
            || apply_entity_prop!(k, v, w, a, e, c, p, BorderColorProperty, BorderColor)
    }

    fn node_from_attrs(
        e: &quick_xml::events::BytesStart,
        pos: usize,
        asset_server: &AssetServer,
        node_data: &mut DuiElt,
        is_div: bool,
    ) -> Result<(), anyhow::Error> {
        let DuiElt {
            ref mut components,
            ref mut id,
            ref mut prop_components,
            ..
        } = node_data;

        macro_rules! ensure {
            ($c:ident, $t:ty) => {
                $c.entry(TypeId::of::<$t>())
                    .or_insert_with(|| Box::<$t>::default().into_reflect());
            };
        }

        ensure!(components, ComputedNode);
        ensure!(components, Node);
        ensure!(components, FocusPolicy);
        ensure!(components, Transform);
        ensure!(components, GlobalTransform);
        ensure!(components, Visibility);
        ensure!(components, InheritedVisibility);
        ensure!(components, ViewVisibility);
        ensure!(components, ZIndex);

        let mut world = World::new();
        let ent = world.spawn_empty().id();

        // process text first so that styles get applied correctly
        if let Some(attr) = e.try_get_attribute("text")? {
            let text_body = if attr.value.starts_with(b"@") {
                prop_components.insert(
                    PropComponent::Text,
                    String::from_utf8_lossy(&attr.value[1..]).into_owned(),
                );
                String::from("lorem ipsum")
            } else {
                String::from_utf8_lossy(&attr.value).into_owned()
            };

            components.insert(
                TypeId::of::<Text>(),
                Box::new(Text::new(text_body)).into_reflect(),
            );
        }

        for attr in e.attributes() {
            let attr = attr?;
            match attr.key.as_ref() {
                b"class" => {
                    components.insert(
                        TypeId::of::<bevy_ecss::Class>(),
                        Box::new(bevy_ecss::Class::new(
                            String::from_utf8_lossy(&attr.value).into_owned(),
                        ))
                        .into_reflect(),
                    );
                }
                b"id" => {
                    let name = String::from_utf8_lossy(&attr.value).into_owned();
                    *id = Some(name.clone());
                    components.insert(
                        TypeId::of::<Name>(),
                        Box::new(Name::new(name)).into_reflect(),
                    );
                }
                b"style" => {
                    let content = format!("#inline {{{}}}", std::str::from_utf8(&attr.value)?);
                    let ss = bevy_ecss::StyleSheetAsset::parse("", &content);
                    for rule in ss.iter() {
                        for (key, value) in rule.properties.iter() {
                            if !Self::apply_props(
                                key,
                                value,
                                &mut world,
                                ent,
                                asset_server,
                                components,
                                &mut Some(prop_components),
                            ) {
                                warn!("unmatched style property {key}");
                            }
                        }
                    }
                }
                b"text" => (), // already handled above
                b"image" => {
                    if attr.value.as_ref().starts_with(b"@") {
                        prop_components.insert(
                            PropComponent::Image,
                            String::from_utf8_lossy(&attr.value[1..]).into_owned(),
                        );
                        components
                            .entry(TypeId::of::<ImageNode>())
                            .or_insert_with(|| Box::new(ImageNode::default()).into_reflect());
                    } else {
                        let image =
                            asset_server.load(String::from_utf8_lossy(&attr.value).into_owned());
                        components
                            .entry(TypeId::of::<ImageNode>())
                            .or_insert_with(|| Box::new(ImageNode::default()).into_reflect())
                            .downcast_mut::<ImageNode>()
                            .unwrap()
                            .image = image;
                    }
                }
                b"image-color" => {
                    if attr.value.as_ref().starts_with(b"@") {
                        prop_components.insert(
                            PropComponent::ImageColor,
                            String::from_utf8_lossy(&attr.value[1..]).into_owned(),
                        );
                        components
                            .entry(TypeId::of::<ImageNode>())
                            .or_insert_with(|| Box::new(ImageNode::default()).into_reflect());
                    } else {
                        let content =
                            format!("#inline {{color='{}'}}", std::str::from_utf8(&attr.value)?);
                        let ss = bevy_ecss::StyleSheetAsset::parse("", &content);
                        if let Some(color) = ss
                            .get_properties(&Selector::new(Default::default()), "color")
                            .and_then(|c| FontColorProperty::parse(c).ok())
                        {
                            components
                                .entry(TypeId::of::<ImageNode>())
                                .or_insert_with(|| Box::new(ImageNode::default()).into_reflect())
                                .downcast_mut::<ImageNode>()
                                .unwrap()
                                .color = color;
                        } else {
                            warn!("failed to parse image-color `{:?}`", attr.value);
                        }
                    }
                }
                b"focus" => {
                    match attr.value.as_ref() {
                        b"pass" => components.insert(
                            TypeId::of::<FocusPolicy>(),
                            Box::new(FocusPolicy::Pass).into_reflect(),
                        ),
                        b"block" => components.insert(
                            TypeId::of::<FocusPolicy>(),
                            Box::new(FocusPolicy::Block).into_reflect(),
                        ),
                        _ => bail!("focus policy must be 'block' or 'pass'"),
                    };
                }
                b"interact" => {
                    components.insert(
                        TypeId::of::<Interaction>(),
                        Box::<Interaction>::default().into_reflect(),
                    );
                }
                b"z-index" => {
                    let index = String::from_utf8_lossy(attr.value.as_ref()).parse::<i32>()?;
                    components.insert(
                        TypeId::of::<ZIndex>(),
                        Box::new(ZIndex(index)).into_reflect(),
                    );
                }
                _ => {
                    if is_div {
                        warn!(
                            "unexpected attribute {} @ {}",
                            String::from_utf8_lossy(attr.key.as_ref()),
                            pos
                        )
                    }
                }
            }
        }

        if !components.contains_key(&TypeId::of::<BackgroundColor>()) {
            // add bg color if not explicitly specified
            components
                .entry(TypeId::of::<BackgroundColor>())
                .or_insert_with(|| Box::new(BackgroundColor::from(Color::NONE)).into_reflect());
        }
        if components.get(&TypeId::of::<BorderColor>()).is_none() {
            components
                .entry(TypeId::of::<BorderColor>())
                .or_insert_with(|| Box::new(BorderColor::from(Color::NONE)).into_reflect());
        }
        // add border radius, required for border to render ... TODO: support this properly
        components
            .entry(TypeId::of::<BorderRadius>())
            .or_insert_with(|| Box::new(BorderRadius::default()).into_reflect());

        Ok(())
    }

    fn read_inner(
        xml: &mut quick_xml::Reader<&[u8]>,
        asset_server: &AssetServer,
    ) -> Result<Option<DuiNode>, anyhow::Error> {
        let mut template = String::default();
        let mut elts = Vec::default();
        let mut open_elts: Vec<(usize, quick_xml::events::BytesStart<'static>)> = Vec::default();

        loop {
            let pos = xml.buffer_position();
            let ev = xml.read_event()?;
            match &ev {
                quick_xml::events::Event::Eof => {
                    if !template.is_empty() {
                        return Err(anyhow!("malformed template"));
                    }

                    return Ok(None);
                }

                quick_xml::events::Event::Empty(e) | quick_xml::events::Event::Start(e) => {
                    let (elt, childtype) = match e.name().as_ref() {
                        b"define-template" => {
                            ensure!(template.is_empty());
                            template = String::from_utf8_lossy(
                                &e.try_get_attribute("id")?
                                    .ok_or(anyhow!(
                                        "define-template must have an `id` attribute @ {pos}"
                                    ))?
                                    .value,
                            )
                            .into_owned();
                            (None, ChildType::SpawnNew)
                        }
                        b"div" => {
                            let mut elt = DuiElt::default();
                            if matches!(ev, quick_xml::events::Event::Empty(_)) {
                                Self::node_from_attrs(e, pos, asset_server, &mut elt, true)?
                            }
                            (Some(elt), ChildType::SpawnNew)
                        }
                        _ => {
                            let (child_type, target_val) = match e.name().as_ref() {
                                b"apply" => {
                                    (
                                        ChildType::ApplyToParent,
                                        e.try_get_attribute("template")?
                                            .ok_or(anyhow!(
                                                "define-template must have a `template` attribute @ {pos}"
                                            ))?
                                            .value
                                            .into_owned()
                                    )
                                }
                                b"apply-children" => (ChildType::ApplyToParent, b"apply-children".to_vec()),
                                _ => (ChildType::SpawnNew, e.name().as_ref().to_owned()),
                            };

                            let target = if target_val.starts_with(b"@") {
                                PropValue::Prop(
                                    String::from_utf8_lossy(&target_val[1..]).into_owned(),
                                )
                            } else {
                                PropValue::Literal(
                                    String::from_utf8_lossy(&target_val).into_owned(),
                                )
                            };
                            let properties = HashMap::from_iter(
                                e.attributes().flat_map(|a| a.ok()).map(|attr| {
                                    let target_prop =
                                        String::from_utf8_lossy(attr.key.as_ref()).into_owned();
                                    let value = if attr.value.starts_with(b"@") {
                                        PropValue::Prop(
                                            String::from_utf8_lossy(&attr.value[1..]).into_owned(),
                                        )
                                    } else {
                                        PropValue::Literal(
                                            String::from_utf8_lossy(&attr.value).into_owned(),
                                        )
                                    };

                                    (target_prop, value)
                                }),
                            );
                            let id = e
                                .try_get_attribute("id")?
                                .map(|attr| String::from_utf8_lossy(&attr.value).into_owned());

                            let mut elt = DuiElt {
                                id,
                                template: Some(target),
                                properties,
                                ..Default::default()
                            };

                            if matches!(ev, quick_xml::events::Event::Empty(_)) {
                                Self::node_from_attrs(e, pos, asset_server, &mut elt, false)?
                            };

                            (Some(elt), child_type)
                        }
                    };
                    ensure!(!template.is_empty());

                    if let Some((ix, _)) = open_elts.last() {
                        if let Some(DuiElt {
                            ref mut children, ..
                        }) = elts[*ix]
                        {
                            children.push(childtype)
                        }
                    }

                    if matches!(ev, quick_xml::events::Event::Start(_)) {
                        open_elts.push((elts.len(), e.clone().into_owned()));
                    }

                    elts.push(elt);
                }

                quick_xml::events::Event::Text(e) => {
                    let Some(Some(DuiElt {
                        ref mut components, ..
                    })) = elts.last_mut()
                    else {
                        bail!("text not expected here @ {}", pos);
                    };
                    components.insert(
                        TypeId::of::<Text>(),
                        Box::new(Text::new(e.unescape()?)).into_reflect(),
                    );
                }

                quick_xml::events::Event::End(_) => {
                    let (ix, e) = open_elts.pop().ok_or(anyhow!("close without open"))?;
                    let Some(ref mut elt) = elts[ix] else {
                        // closing the define-template
                        let None = elts.remove(0) else {
                            panic!();
                        };
                        return Ok(Some(DuiNode {
                            template,
                            nodes: elts.into_iter().map(Option::unwrap).collect(),
                        }));
                    };
                    Self::node_from_attrs(&e, pos, asset_server, elt, false)?;
                }

                // skip
                quick_xml::events::Event::Comment(_)
                | quick_xml::events::Event::Decl(_)
                | quick_xml::events::Event::PI(_)
                | quick_xml::events::Event::CData(_)
                | quick_xml::events::Event::DocType(_) => (),
            }
        }
    }
}

pub struct DuiLoader {
    asset_server: AssetServer,
}

impl AssetLoader for DuiLoader {
    type Asset = DuiNodeList;

    type Settings = ();

    type Error = anyhow::Error;

    fn load(
        &self,
        reader: &mut dyn bevy::asset::io::Reader,
        _: &Self::Settings,
        context: &mut bevy::asset::LoadContext,
    ) -> impl ConditionalSendFuture<Output = Result<Self::Asset, Self::Error>> {
        Box::pin(async move {
            let res = async {
                let mut buf = Vec::default();
                reader.read_to_end(&mut buf).await?;
                let mut xml = quick_xml::Reader::from_str(std::str::from_utf8(&buf)?);
                xml.trim_text(true);

                let mut list = Vec::default();
                while let Some(node) = Self::read_inner(&mut xml, &self.asset_server)? {
                    list.push(node);
                }

                Ok(DuiNodeList(list))
            }
            .await;
            if let Err(e) = &res {
                warn!("error processing dui template `{:?}`: {e}", context.path());
            }
            res
        })
    }

    fn extensions(&self) -> &[&str] {
        &["dui"]
    }
}

pub struct DuiPlugin;

impl Plugin for DuiPlugin {
    fn build(&self, app: &mut App) {
        let asset_server = app.world().resource::<AssetServer>().clone();
        app.init_asset::<DuiNodeList>()
            .register_asset_loader(DuiLoader { asset_server })
            .init_resource::<DuiRegistry>()
            .add_systems(PreUpdate, add_duis);
    }
}

fn add_duis(
    mut evs: EventReader<AssetEvent<DuiNodeList>>,
    assets: Res<Assets<DuiNodeList>>,
    mut dui: ResMut<DuiRegistry>,
) {
    for ev in evs.read() {
        match ev {
            AssetEvent::Added { id } | AssetEvent::Modified { id } => {
                let Some(list) = assets.get(*id) else {
                    continue;
                };
                for node in list.0.iter() {
                    dui.register_template(node.template.clone(), node.clone());
                }
            }
            _ => (),
        }
    }
}

pub trait DuiCommandsExt {
    fn spawn_template(
        &mut self,
        dui: &DuiRegistry,
        template: &str,
        props: DuiProps,
    ) -> Result<DuiEntities, anyhow::Error>;
}

impl<'w, 's> DuiCommandsExt for Commands<'w, 's> {
    fn spawn_template(
        &mut self,
        dui: &DuiRegistry,
        template: &str,
        props: DuiProps,
    ) -> Result<DuiEntities, anyhow::Error> {
        self.spawn(Node::default())
            .apply_template(dui, template, props)
    }
}

impl<'a> DuiCommandsExt for ChildSpawnerCommands<'a> {
    fn spawn_template(
        &mut self,
        dui: &DuiRegistry,
        template: &str,
        props: DuiProps,
    ) -> Result<DuiEntities, anyhow::Error> {
        let mut root = self.spawn(Node::default());
        dui.apply_template(&mut root, template, props)
    }
}

pub trait DuiEntityCommandsExt {
    fn apply_template(
        &mut self,
        dui: &DuiRegistry,
        template: &str,
        props: DuiProps,
    ) -> Result<DuiEntities, anyhow::Error>;
}

impl<'a> DuiCommandsExt for EntityCommands<'a> {
    fn spawn_template(
        &mut self,
        dui: &DuiRegistry,
        template: &str,
        props: DuiProps,
    ) -> Result<DuiEntities, anyhow::Error> {
        let results = self.commands().spawn_template(dui, template, props)?;
        self.add_children(&[results.root]);
        Ok(results)
    }
}

impl<'a> DuiEntityCommandsExt for EntityCommands<'a> {
    fn apply_template(
        &mut self,
        dui: &DuiRegistry,
        template: &str,
        props: DuiProps,
    ) -> Result<DuiEntities, anyhow::Error> {
        dui.apply_template(self, template, props)
    }
}

pub struct DuiComponentFromClone<C: Component + Clone>(String, PhantomData<fn() -> C>);
impl<C: Component + Clone> DuiComponentFromClone<C> {
    pub fn new(prop_name: impl Into<String>) -> Self {
        Self(prop_name.into(), PhantomData)
    }
}
impl<C: Component + Clone> DuiTemplate for DuiComponentFromClone<C> {
    fn render(
        &self,
        commands: &mut EntityCommands,
        props: DuiProps,
        ctx: &mut DuiContext,
    ) -> Result<NodeMap, anyhow::Error> {
        let component = props
            .borrow::<C>(&self.0, ctx)?
            .ok_or(anyhow::anyhow!("no value provided"))?;
        commands.insert(component.clone());
        Ok(Default::default())
    }
}

pub struct DuiComponentFromValue<C: Component, V>(String, PhantomData<fn() -> (C, V)>);

impl<C: Component, V: 'static> DuiComponentFromValue<C, V> {
    pub fn new(prop_name: impl Into<String>) -> Self {
        Self(prop_name.into(), PhantomData)
    }
}

impl<C: Component, V: 'static> DuiTemplate for DuiComponentFromValue<C, V>
where
    C: for<'a> TryFrom<&'a V>,
    for<'a> <C as TryFrom<&'a V>>::Error: std::fmt::Display,
{
    fn render(
        &self,
        commands: &mut EntityCommands,
        props: DuiProps,
        ctx: &mut DuiContext,
    ) -> Result<NodeMap, anyhow::Error> {
        let value_str = props.borrow::<V>(&self.0, ctx)?;
        let component = value_str
            .map(|v| C::try_from(v))
            .transpose()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .ok_or(anyhow::anyhow!("missing value property"))?;
        commands.insert(component);
        Ok(Default::default())
    }
}

#[derive(Default)]
pub struct DuiMarkerComponent<C: Component + Default>(PhantomData<fn() -> C>);

impl<C: Component + Default> DuiTemplate for DuiMarkerComponent<C> {
    fn render(
        &self,
        commands: &mut EntityCommands,
        _: DuiProps,
        _: &mut DuiContext,
    ) -> Result<NodeMap, anyhow::Error> {
        commands.insert(C::default());
        Ok(Default::default())
    }
}

#[derive(SystemParam)]
pub struct DuiWalker<'w, 's> {
    q: Query<'w, 's, &'static DuiEntities>,
}

impl<'w, 's> DuiWalker<'w, 's> {
    pub fn walk(&self, from: Entity, path: impl Into<String>) -> Option<Entity> {
        let path = path.into();

        debug!("walking {path}");

        let mut target = from;
        for segment in path.split('.') {
            let Ok(entities) = self.q.get(target) else {
                debug!("no entities :(");
                return None;
            };
            let Some(next) = entities.get_named(segment) else {
                debug!(
                    "failed to find segment {segment}, available names were: {:?}",
                    entities.named_nodes.keys()
                );
                return None;
            };
            target = next;
            debug!("  found segment {segment} -> {target:?}");
        }

        Some(target)
    }
}
