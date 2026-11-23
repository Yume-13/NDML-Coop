// MIT License
//
// Copyright © 2026 Yume-13
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Every GML identifier the Steam connector uses (Nova Drift 1.2.38.4).
//! Objects and rooms are looked up by name at run time, never by index.

#![forbid(unsafe_code)]

/// Begin Step of the persistent `oGamepad`, which ticks every frame in every
/// room, also during pause and upgrade freezes: the pump.
pub const PUMP_EVENT: &str = "gml_Object_oGamepad_Step_1";
/// Draw GUI End of `oGuiCursor`, the frame's last event: the flush.
pub const FLUSH_EVENT: &str = "gml_Object_oGuiCursor_Draw_75";

/// The only object with an Async Steam event (`Other_69`); one hidden,
/// persistent instance is the listener.
pub const LISTENER_OBJECT: &str = "oHighscoresDynamic";
/// Its Create: skipped while the mod creates the listener.
pub const LISTENER_CREATE: &str = "gml_Object_oHighscoresDynamic_Create_0";
/// Its Async Steam event: where `async_load` is read.
pub const LISTENER_ASYNC: &str = "gml_Object_oHighscoresDynamic_Other_69";
/// Its other events, skipped for the hidden instance.
pub const LISTENER_EVENTS: [&str; 6] = [
    "gml_Object_oHighscoresDynamic_Step_0",
    "gml_Object_oHighscoresDynamic_Alarm_0",
    "gml_Object_oHighscoresDynamic_Alarm_1",
    "gml_Object_oHighscoresDynamic_Alarm_2",
    "gml_Object_oHighscoresDynamic_Draw_64",
    "gml_Object_oHighscoresDynamic_KeyPress_123",
];
/// Leaderboard code that reaches `oHighscoresDynamic`: the listener is
/// destroyed before these run.
pub const LEADERBOARD_GUARDS: [&str; 3] = [
    "gml_Object_oToggleLeaderBoard_Step_0",
    "gml_Object_oToggleLeaderBoard_Draw_64",
    "gml_Object_oHighscoreEntry_Step_0",
];
/// Objects whose presence keeps the listener away.
pub const LEADERBOARD_OBJECTS: [&str; 2] = ["oToggleLeaderBoard", "oHighscoreEntry"];
/// The leaderboard room (room 12 in 1.2.38.4): no listener there.
pub const ROOM_LEADERBOARD: &str = "roomHighscoresDynamic";
/// The run room (room 10): the host's lobby says `state = in_run` there.
pub const ROOM_GAME: &str = "roomGame";
/// The script the game creates instances with (as the hook's `self`).
pub const INSTANCE_CREATE: &str = "gml_Script_instance_create";
/// `global.gog`: the non-Steam build path; every Steam call is a stub there.
pub const GLOBAL_GOG: &str = "gog";
pub const VAR_ROOM: &str = "room";
/// The async result map, read on the listener.
pub const VAR_ASYNC_LOAD: &str = "async_load";

/// Steamworks extension functions (GMEXT-Steamworks 1.6.9).
pub mod steam_fn {
    pub const INITIALISED: &str = "steam_initialised";
    pub const MY_ID: &str = "steam_get_user_steam_id";
    pub const UPDATE: &str = "steam_update";
    pub const OVERLAY_ENABLED: &str = "steam_is_overlay_enabled";
    pub const LOBBY_CREATE: &str = "steam_lobby_create";
    pub const LOBBY_JOIN: &str = "steam_lobby_join_id";
    pub const LOBBY_LEAVE: &str = "steam_lobby_leave";
    pub const LOBBY_SET_TYPE: &str = "steam_lobby_set_type";
    pub const LOBBY_SET_OWNER: &str = "steam_lobby_set_owner_id";
    pub const LOBBY_ID: &str = "steam_lobby_get_lobby_id";
    pub const LOBBY_OWNER: &str = "steam_lobby_get_owner_id";
    pub const LOBBY_MEMBERS: &str = "steam_lobby_get_member_count";
    pub const LOBBY_MEMBER: &str = "steam_lobby_get_member_id";
    pub const LOBBY_SET_DATA: &str = "steam_lobby_set_data";
    pub const LOBBY_GET_DATA: &str = "steam_lobby_get_data";
    pub const LOBBY_INVITE: &str = "steam_lobby_activate_invite_overlay";
    pub const LIST_STRING_FILTER: &str = "steam_lobby_list_add_string_filter";
    pub const LIST_DISTANCE_FILTER: &str = "steam_lobby_list_add_distance_filter";
    pub const LIST_REQUEST: &str = "steam_lobby_list_request";
    pub const LIST_LOADING: &str = "steam_lobby_list_is_loading";
    pub const LIST_COUNT: &str = "steam_lobby_list_get_count";
    pub const LIST_JOIN: &str = "steam_lobby_list_join";
    pub const PACKET_SEND: &str = "steam_net_packet_send";
    pub const PACKET_RECEIVE: &str = "steam_net_packet_receive";
    pub const PACKET_SIZE: &str = "steam_net_packet_get_size";
    pub const PACKET_DATA: &str = "steam_net_packet_get_data";
    pub const PACKET_SENDER: &str = "steam_net_packet_get_sender_id";
    pub const PERSONA_NAME: &str = "steam_get_user_persona_name_sync";
}

/// Runner builtins.
pub mod builtin_fn {
    pub const BUFFER_CREATE: &str = "buffer_create";
    pub const BUFFER_EXISTS: &str = "buffer_exists";
    pub const BUFFER_B64_DECODE_EXT: &str = "buffer_base64_decode_ext";
    pub const BUFFER_B64_ENCODE: &str = "buffer_base64_encode";
    pub const INSTANCE_EXISTS: &str = "instance_exists";
    pub const INSTANCE_ACTIVATE_OBJECT: &str = "instance_activate_object";
    pub const INSTANCE_DESTROY: &str = "instance_destroy";
    pub const ASSET_GET_INDEX: &str = "asset_get_index";
    pub const DS_MAP_FIND_VALUE: &str = "ds_map_find_value";
}

/// `buffer_grow`.
pub const BUFFER_GROW: f64 = 1.0;
/// Unreliable send type: ndml-net does its own reliability.
pub const SEND_UNRELIABLE: f64 = 0.0;
/// `ELobbyComparison::Equal`.
pub const LIST_EQUAL: f64 = 0.0;
/// `ELobbyDistanceFilter::Worldwide`.
pub const LIST_WORLDWIDE: f64 = 3.0;
