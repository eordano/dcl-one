use bevy::prelude::*;
use bevy_ecss::prelude::{Class, EcssPlugin, StyleSheet};

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(EcssPlugin::default())
        .add_systems(Startup, setup)
        .run();
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    // Camera
    commands.spawn(Camera2d::default());

    // root node
    commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::SpaceBetween,
                ..default()
            },
            BackgroundColor::from(Color::NONE),
        ))
        .insert(StyleSheet::new(asset_server.load("sheets/simple_ui.css")))
        .insert(Name::new("ui-root"))
        .with_children(|parent| {
            // left vertical fill (border)
            parent
                .spawn((
                    Node {
                        width: Val::Px(200.0),
                        height: Val::Percent(100.0),
                        border: UiRect::all(Val::Px(2.0)),
                        ..default()
                    },
                    BackgroundColor::from(Color::srgb(0.65, 0.65, 0.65)),
                ))
                .insert(Name::new("left-border"))
                .with_children(|parent| {
                    // left vertical fill (content)
                    parent
                        .spawn((
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Percent(100.0),
                                align_items: AlignItems::FlexEnd,
                                ..default()
                            },
                            BackgroundColor::from(Color::srgb(0.15, 0.15, 0.15)),
                        ))
                        .insert(Name::new("left-bg"))
                        .with_children(|parent| {
                            // text
                            parent
                                .spawn((
                                    Text::new("Text Example"),
                                    TextFont {
                                        font: asset_server.load("fonts/FiraSans-Bold.ttf"),
                                        font_size: 30.0,
                                        ..Default::default()
                                    },
                                    TextColor(Color::WHITE),
                                    Node {
                                        margin: UiRect::all(Val::Px(5.0)),
                                        ..default()
                                    },
                                ))
                                .insert(Name::new("left-text"));
                        });
                });
            // right vertical fill
            parent
                .spawn((
                    Node {
                        flex_direction: FlexDirection::ColumnReverse,
                        justify_content: JustifyContent::Center,
                        width: Val::Px(200.0),
                        height: Val::Percent(100.0),
                        ..default()
                    },
                    BackgroundColor::from(Color::srgb(0.15, 0.15, 0.15)),
                ))
                .insert(Name::new("right-border"))
                .with_children(|parent| {
                    // Title
                    parent
                        .spawn((
                            Text::new("Scrolling list"),
                            TextFont {
                                font: asset_server.load("fonts/FiraSans-Bold.ttf"),
                                font_size: 25.,
                                ..Default::default()
                            },
                            TextColor(Color::WHITE),
                            Node {
                                height: Val::Px(25.0),
                                margin: UiRect {
                                    left: Val::Auto,
                                    right: Val::Auto,
                                    ..default()
                                },
                                ..default()
                            },
                        ))
                        .insert(Name::new("right-bg"));
                    // List with hidden overflow
                    parent
                        .spawn((
                            Node {
                                flex_direction: FlexDirection::ColumnReverse,
                                align_self: AlignSelf::Center,
                                width: Val::Percent(100.0),
                                height: Val::Percent(50.0),
                                overflow: Overflow::clip(),
                                ..default()
                            },
                            BackgroundColor::from(Color::srgb(0.10, 0.10, 0.10)),
                        ))
                        .insert(Name::new("right-list"))
                        .with_children(|parent| {
                            // Moving panel
                            parent
                                .spawn((
                                    Node {
                                        flex_direction: FlexDirection::ColumnReverse,
                                        flex_grow: 1.0,
                                        ..default()
                                    },
                                    BackgroundColor::from(Color::NONE),
                                ))
                                .insert(Name::new("right-moving-panel"))
                                .with_children(|parent| {
                                    // List items
                                    for i in 0..30 {
                                        parent
                                            .spawn((
                                                Text::new(format!("Item {i}")),
                                                TextFont {
                                                    font: asset_server
                                                        .load("fonts/FiraSans-Bold.ttf"),
                                                    font_size: 20.,
                                                    ..Default::default()
                                                },
                                                TextColor(Color::WHITE),
                                                Node {
                                                    flex_shrink: 0.,
                                                    height: Val::Px(20.),
                                                    margin: UiRect {
                                                        left: Val::Auto,
                                                        right: Val::Auto,
                                                        ..default()
                                                    },
                                                    ..default()
                                                },
                                            ))
                                            .insert(Class::new("big-text"))
                                            .insert(Name::new(format!("right-item-{}", i)));
                                    }
                                });
                        });
                });
            // absolute positioning
            parent
                .spawn((
                    Node {
                        width: Val::Px(200.0),
                        height: Val::Px(200.0),
                        position_type: PositionType::Absolute,
                        left: Val::Px(210.0),
                        bottom: Val::Px(10.0),
                        border: UiRect::all(Val::Px(20.0)),
                        ..default()
                    },
                    BackgroundColor::from(Color::srgb(0.4, 0.4, 1.0)),
                ))
                .insert(Name::new("mid-blue-border"))
                .with_children(|parent| {
                    parent
                        .spawn((
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Percent(100.0),
                                ..default()
                            },
                            BackgroundColor::from(Color::srgb(0.8, 0.8, 1.0)),
                        ))
                        .insert(Name::new("mid-navy-blue-content"));
                });
            // render order test: reddest in the back, whitest in the front (flex center)
            parent
                .spawn((
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        position_type: PositionType::Absolute,
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::Center,
                        ..default()
                    },
                    BackgroundColor::from(Color::NONE),
                ))
                .insert(Name::new("mid-red-last"))
                .insert(Class::new("blue-bg container"))
                .with_children(|parent| {
                    parent
                        .spawn((
                            Node {
                                width: Val::Px(100.0),
                                height: Val::Px(100.0),
                                ..default()
                            },
                            BackgroundColor::from(Color::srgb(1.0, 0.0, 0.0)),
                        ))
                        .insert(Name::new("mid-red-last-but-one"))
                        .with_children(|parent| {
                            parent
                                .spawn((
                                    Node {
                                        width: Val::Px(100.0),
                                        height: Val::Px(100.0),
                                        position_type: PositionType::Absolute,
                                        left: Val::Px(20.0),
                                        bottom: Val::Px(20.0),
                                        ..default()
                                    },
                                    BackgroundColor::from(Color::srgb(1.0, 0.3, 0.3)),
                                ))
                                .insert(Name::new("mid-red-center"));
                            parent
                                .spawn((
                                    Node {
                                        width: Val::Px(100.0),
                                        height: Val::Px(100.0),
                                        position_type: PositionType::Absolute,
                                        left: Val::Px(40.0),
                                        bottom: Val::Px(40.0),
                                        ..default()
                                    },
                                    BackgroundColor::from(Color::srgb(1.0, 0.5, 0.5)),
                                ))
                                .insert(Class::new("blue-bg"))
                                .insert(Name::new("mid-red-top-but-one"));
                            parent
                                .spawn((
                                    Node {
                                        width: Val::Px(100.0),
                                        height: Val::Px(100.0),
                                        position_type: PositionType::Absolute,
                                        left: Val::Px(60.0),
                                        bottom: Val::Px(60.0),
                                        ..default()
                                    },
                                    BackgroundColor::from(Color::srgb(1.0, 0.7, 0.7)),
                                ))
                                .insert(Name::new("mid-red-top"));
                            // alpha test
                            parent
                                .spawn((
                                    Node {
                                        width: Val::Px(100.0),
                                        height: Val::Px(100.0),
                                        position_type: PositionType::Absolute,
                                        left: Val::Px(80.0),
                                        bottom: Val::Px(80.0),
                                        ..default()
                                    },
                                    BackgroundColor::from(Color::srgba(1.0, 0.9, 0.9, 0.4)),
                                ))
                                .insert(Class::new("blue-bg"))
                                .insert(Name::new("mid-red-alpha"));
                        });
                });
            // bevy logo (flex center)
            parent
                .spawn((
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        position_type: PositionType::Absolute,
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::FlexEnd,
                        ..default()
                    },
                    BackgroundColor::from(Color::NONE),
                ))
                .insert(Name::new("mid-bevy-logo-bg"))
                .with_children(|parent| {
                    // bevy logo (image)
                    parent.spawn((
                        ImageNode::new(asset_server.load("branding/bevy_logo_dark_big.png")),
                        Node {
                            width: Val::Px(500.0),
                            ..default()
                        },
                        Name::new("mid-bevy-logo-image"),
                    ));
                });
        });
}
