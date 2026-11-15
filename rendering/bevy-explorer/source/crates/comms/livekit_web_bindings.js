function log(...args) {
    console.log("[livekit]", ...args)
}
function warn(...args) {
    console.warn("[livekit]", ...args)
}
function error(...args) {
    console.error("[livekit]", ...args)
}

var audioContext = null;
var microphonePermission = "denied";
const roomPlayback = new WeakMap();

/**
 * @param {function} on_change called with the permission state now and whenever it changes
 */
export function setupMicrophonePermission(on_change) {
    navigator.permissions.query({ name: "microphone" }).then((permissionState) => {
        microphonePermission = permissionState.state;
        on_change(microphonePermission);

        permissionState.onchange = () => {
            microphonePermission = permissionState.state;
            on_change(microphonePermission);
        };
    });
}

/**
 * Tests if the browser can accept requests for microphone streams
 * @returns boolean
 */
export function is_microphone_available() {
    return !!(navigator.mediaDevices && navigator.mediaDevices.getUserMedia);
}

/**
 * Requests current microphone permission state
 * @returns "granted" | "prompt" | "denied"
 */
export function microphonePermissionState() {
    return microphonePermission;
}

/**
 * Prompts for microphone permission
 */
export function promptMicrophonePermission() {
    if (navigator.mediaDevices && navigator.mediaDevices.getUserMedia) {
        navigator.mediaDevices.getUserMedia({ audio: true });
    }
}

/**
 * 
 * @param {string} url
 * @param {string} token
 * @param {livekit.RoomOptions} room_options 
 * @param {livekit.RoomConnectOptions} room_connect_options 
 * @param {function} handler 
 * @returns livekit.Room
 */
export async function room_connect(url, token, room_options, room_connect_options, handler) {
    // The page supplies the only audio graph. SDK Web Audio mixing would bypass its gain.
    const room = new LivekitClient.Room({ ...room_options, webAudioMix: false });
    const tracks = new Set();
    roomPlayback.set(room, tracks);
    set_room_event_handler(room, handler, tracks);

    try {
        await room.connect(url, token, room_connect_options);
    } catch (err) {
        release_room_playback(room);
        throw err;
    }

    return room;
}

/**
 * 
 * @param {livekit.Room} room
 */
export async function room_close(room) {
    await room.disconnect();
}

/**
 * Releases a room the engine dropped: its event handler is gone.
 * @param {livekit.Room} room
 */
export function room_drop(room) {
    // A connect may finish after its engine owner has gone. No later Close command
    // is guaranteed, so releasing the page handle must also close the SDK session.
    if (release_room_playback(room)) {
        void room.disconnect().catch(() => {});
    }
}

function release_room_playback(room) {
    const tracks = roomPlayback.get(room);
    if (tracks === null) return false;
    roomPlayback.set(room, null);
    // Disconnect may emit unsubscribe events after the Rust event closure is gone.
    room.removeAllListeners();
    tracks?.forEach(drop_track_playback);
    tracks?.clear();
    return true;
}

/**
 * 
 * @param {livekit.Room} room
 * @returns string
 */
export function room_name(room) {
    return room.name
}

/**
 *
 * @param {livekit.Room} room
 * @returns string
 */
export function room_metadata(room) {
    return room.metadata ?? ''
}

/**
 * 
 * @param {livekit.Room} room
 * @returns livekit.LocalParticipant
 */
export function room_local_participant(room) {
    return room.localParticipant;
}

/**
 * 
 * @param {livekit.Room} room 
 * @param {function} handler 
 */
function set_room_event_handler(room, handler, tracks) {
    room.on(LivekitClient.RoomEvent.Connected, () => {
        const participants_with_tracks = Array
            .from(room.remoteParticipants.values())
            .filter(remote_participant => room.localParticipant.sid != remote_participant.sid)
            .map(remote_participant => {
                return {
                    participant: remote_participant,
                    tracks: Array.from(remote_participant.trackPublications.values())
                };
            });
        handler({
            type: 'connected',
            participants_with_tracks
        })
    });
    room.on(LivekitClient.RoomEvent.Disconnected, (disconnectReason) => {
        handler({
            type: 'disconnected',
            disconnectReason
        })
    });
    room.on(LivekitClient.RoomEvent.ConnectionStateChanged, (state) => {
        handler({
            type: 'connectionStateChanged',
            state: state
        })
    });
    room.on(LivekitClient.RoomEvent.ConnectionQualityChanged, (connection_quality, participant) => {
        handler({
            type: 'connectionQualityChanged',
            connection_quality,
            participant
        })
    });
    room.on(
        LivekitClient.RoomEvent.DataReceived,
        (payload, participant, kind, topic, _encryptionType, participantIdentity) => {
            handler({
                type: 'dataReceived',
                payload,
                participant,
                participantIdentity,
                kind,
                topic
            })
        }
    );
    room.on(
        LivekitClient.RoomEvent.ParticipantConnected,
        (remote_participant) => {
            handler({
                type: 'participantConnected',
                participant: remote_participant,
            })
        }
    );
    room.on(
        LivekitClient.RoomEvent.ParticipantDisconnected,
        (remote_participant) => {
            handler({
                type: 'participantDisconnected',
                participant: remote_participant,
            })
        }
    );
    room.on(
        LivekitClient.RoomEvent.ParticipantMetadataChanged,
        (prev_metadata, participant) => {
            handler({
                type: 'participantMetadataChanged',
                participant,
                old_metadata: prev_metadata,
                metadata: participant.metadata
            })
        }
    );
    room.on(
        LivekitClient.RoomEvent.RoomMetadataChanged,
        (metadata) => {
            handler({
                type: 'roomMetadataChanged',
                metadata: metadata ?? ''
            })
        }
    );
    room.on(
        LivekitClient.RoomEvent.TrackPublished,
        (remote_track_publication, remote_participant) => {
            handler({
                type: 'trackPublished',
                publication: remote_track_publication,
                participant: remote_participant
            })
        }
    );
    room.on(
        LivekitClient.RoomEvent.TrackUnpublished,
        (remote_track_publication, remote_participant) => {
            handler({
                type: 'trackUnpublished',
                publication: remote_track_publication,
                participant: remote_participant
            })
        }
    );
    room.on(
        LivekitClient.RoomEvent.TrackSubscribed,
        (remote_track, remote_track_publication, remote_participant) => {
            log(`Subscribed to track ${remote_track.sid} of ${remote_participant.sid} (${remote_participant.identity}).`);

            if (tracks.has(remote_track)) {
                error(`Rebuilding playback of ${remote_track.sid} for ${remote_participant.sid} (${remote_participant.identity}).`);
                drop_track_playback(remote_track);
            }
            tracks.add(remote_track);
            if (remote_track.kind === "audio") {
                if (!audioContext) {
                    audioContext = new (window.AudioContext || window.webkitAudioContext)();
                }

                if (remote_participant.identity.endsWith("-streamer") || remote_participant.identity.startsWith("stream:") || remote_participant.identity.startsWith("presentation-bot:")) {
                    const audioElement = remote_track.attach();
                    audioElement.volume = 0;
                    remote_track.audioElement = audioElement;
                } else {
                    track_rig_new(remote_track);
                }
            } else if (remote_track.kind == "video") {
                const videoElement = remote_track.attach();
                remote_track.videoElement = videoElement;
            }

            handler({
                type: 'trackSubscribed',
                track: remote_track,
                publication: remote_track_publication,
                participant: remote_participant
            })
        }
    );
    room.on(
        LivekitClient.RoomEvent.TrackUnsubscribed,
        // Note: The browser livekit docs say that the first parameter is a Livekit.Track,
        // not a Livekit.RemoteTrack, verify if there is ever an event with a local
        // track
        (remote_track, remote_track_publication, remote_participant) => {
            log(`Unsubscribed to track ${remote_track.sid} of ${remote_participant.sid} (${remote_participant.identity}).`);
            tracks.delete(remote_track);
            drop_track_playback(remote_track);

            handler({
                type: 'trackUnsubscribed',
                track: remote_track,
                publication: remote_track_publication,
                participant: remote_participant
            })
        }
    );
    room.on(
        LivekitClient.RoomEvent.ActiveSpeakersChanged,
        (speakers) => {
            handler({
                type: 'activeSpeakersChanged',
                speakers,
            })
        }
    );
}

/**
 * 
 * @param {livekit.Participant} participant
 * @returns bool
 */
export async function participant_is_local(participant) {
    return participant.isLocal;
}

/**
 *
 * @param {livekit.LocalParticipant} local_participant
 * @param {Uint8Array} payload
 * @param {boolean} reliable
 * @param {string | undefined} topic
 * @param {string[]} destination_identities
 */
export async function local_participant_publish_data(local_participant, payload, reliable, topic, destination_identities) {
    await local_participant.publishData(payload, {
        reliable,
        topic: topic ?? undefined,
        destinationIdentities: destination_identities,
    });
}

/**
 * 
 * @param {livekit.LocalParticipant} local_participant
 * @param {livekit.LocalTrack} local_track
 * @param {livekit.TrackPublishingOptions} track_publishing_option
 * @returns livekit.LocalTrackPublication
 */
export async function local_participant_publish_track(local_participant, local_track, track_publishing_option) {
    return await local_participant.publishTrack(local_track, track_publishing_option);
}

/**
 * Whether the room's token lets this participant publish at all. A listen-only
 * room (a stage, a world the token admits as audience) says no, and the host
 * then refuses the publish request (`src/livekit/web/host.rs`), so no track of
 * this participant reaches the SDK.
 */
export function local_participant_can_publish(local_participant) {
    return local_participant.permissions?.canPublish ?? true;
}

/**
 * 
 * @param {livekit.LocalParticipant} local_participant
 * @param {livekit.LocalTrack} local_track
 * @returns livekit.LocalTrackPublication
 */
export async function local_participant_unpublish_track(local_participant, local_track) {
    return await local_participant.unpublishTrack(local_track, true);
}

/**
 * 
 * @param {livekit.LocalParticipant} local_participant 
 * @returns bool
 */
export function local_participant_is_local(local_participant) {
    return local_participant.isLocal;
}

/**
 * 
 * @param {livekit.LocalParticipant} local_participant 
 * @returns string
 */
export function local_participant_sid(local_participant) {
    return local_participant.sid;
}

/**
 * 
 * @param {livekit.LocalParticipant} local_participant 
 * @returns string
 */
export function local_participant_identity(local_participant) {
    return local_participant.identity;
}

/**
 * 
 * @param {livekit.LocalParticipant} local_participant 
 * @returns string
 */
export function local_participant_metadata(local_participant) {
    return local_participant.metadata;
}

/**
 * 
 * @param {livekit.LocalParticipant} remote_participant 
 * @returns bool
 */
export function remote_participant_is_local(remote_participant) {
    return remote_participant.isLocal;
}

/**
 * 
 * @param {livekit.RemoteParticipant} remote_participant 
 * @returns string
 */
export function remote_participant_sid(remote_participant) {
    return remote_participant.sid;
}

/**
 * 
 * @param {livekit.RemoteParticipant} remote_participant 
 * @returns string
 */
export function remote_participant_identity(remote_participant) {
    return remote_participant.identity;
}

/**
 * 
 * @param {livekit.RemoteParticipant} remote_participant 
 * @returns string
 */
export function remote_participant_metadata(remote_participant) {
    return remote_participant.metadata;
}

/**
 * 
 * @param {livekit.RemoteTrackPublication} remote_track_publication 
 * @returns string
 */
export function remote_track_publication_sid(remote_track_publication) {
    return remote_track_publication.trackSid;
}

/**
 * 
 * @param {livekit.RemoteTrackPublication} remote_track_publication 
 * @returns string
 */
export function remote_track_publication_kind(remote_track_publication) {
    return remote_track_publication.kind;
}

/**
 * 
 * @param {livekit.RemoteTrackPublication} remote_track_publication 
 * @returns string
 */
export function remote_track_publication_source(remote_track_publication) {
    return remote_track_publication.source;
}

/**
 * 
 * @param {livekit.RemoteTrackPublication} remote_track_publication 
 * @param {boolean} subscribed 
 * @returns string
 */
export function remote_track_publication_set_subscribed(remote_track_publication, subscribed) {
    remote_track_publication.setSubscribed(subscribed);
}

/**
 * 
 * @param {livekit.RemoteTrackPublication} remote_track_publication 
 * @returns livekit.RemoteTrack | null
 */
export function remote_track_publication_track(remote_track_publication) {
    return remote_track_publication.track;
}

/**
 * @param {livekit.LocalTrackPublication} local_track_publication
 * @returns string
 */
export function local_track_publication_sid(local_track_publication) {
    return local_track_publication.trackSid;
}

/**
 * @param {livekit.LocalTrackPublication} local_track_publication
 * @returns string
 */
export function local_track_publication_kind(local_track_publication) {
    return local_track_publication.kind;
}

/**
 * @param {livekit.LocalTrackPublication} local_track_publication
 * @returns string
 */
export function local_track_publication_source(local_track_publication) {
    return local_track_publication.source;
}

/**
 * 
 * @param {livekit.AudioCaptureOptions} options 
 * @returns livekit.LocalAudioTrack
 */
export async function local_audio_track_new(options) {
    try {
        return await LivekitClient.createLocalAudioTrack(options);
    } catch (err) {
        error(err);
    }
}

// Disable the actual capture source synchronously. LiveKit's async signaling mute is not a
// routing barrier: a room handoff must silence the old source before publishing the next one.
export function local_audio_track_set_enabled(track, enabled) {
    track.mediaStreamTrack.enabled = enabled;
}

export function local_audio_track_stop(track) {
    track.mediaStreamTrack.enabled = false;
    track.stop();
}

/**
 * 
 * @param {livekit.RemoteTrack} remote_track 
 */
function track_rig_new(remote_track) {
    log(`Creating new track rig for ${remote_track.sid}.`);

    // Keep the SDK attachment for playback lifecycle, but route sound through the gain below.
    const audioElement = remote_track.attach();
    audioElement.muted = true;

    // use the track internal stream in playback
    const stream = new MediaStream([remote_track.mediaStreamTrack]);
    const source = audioContext.createMediaStreamSource(stream);
    const pannerNode = audioContext.createStereoPanner();
    const gainNode = audioContext.createGain();
    // The engine applies playback gain after subscription arrives.
    gainNode.gain.value = 0;

    // Connect the audio graph: source -> panner -> gain -> destination
    source.connect(pannerNode);
    pannerNode.connect(gainNode);
    gainNode.connect(audioContext.destination);

    // Store the nodes for later control
    remote_track.trackRig = {
        audioElement,
        source,
        pannerNode,
        gainNode,
        stream,
    };

    audioElement.play();
}

/**
 * 
 * @param {livekit.RemoteTrack} remote_track 
 */
function track_rig_drop(remote_track) {
    const track_rig = remote_track.trackRig;
    if (track_rig) {
        log(`Dropping track rig of ${remote_track.sid}.`);
        delete remote_track.trackRig;

        remote_track.detach(track_rig.audioElement);
        track_rig.source.disconnect();
        track_rig.pannerNode.disconnect();
        track_rig.gainNode.disconnect();
        track_rig.audioElement.pause();
    }
}

function drop_track_playback(track) {
    track_rig_drop(track);
    for (const key of ["audioElement", "videoElement"]) {
        const element = track[key];
        if (element) {
            delete track[key];
            track.detach(element);
            element.remove();
        }
    }
}

/**
 * 
 * @param {livekit.RemoteTrack} remote_track 
 * @param {float} pan 
 * @param {float} volume 
 */
export function remote_track_pan_and_volume(remote_track, pan, volume) {
    // log(`Setting pan and volume for track ${remote_track.sid}.`);
    const track_rig = remote_track.trackRig;
    if (!track_rig) return;
    // kira's 0 (left) .. 1 (right) to the stereo panner's -1 .. 1
    track_rig.pannerNode.pan.value = Math.max(-1, Math.min(1, pan * 2 - 1));
    // Volume should be between 0 and 1 (or higher for boost)
    track_rig.gainNode.gain.value = Math.max(0, volume);

    // nodes.analyser.getByteTimeDomainData(nodes.dataArray);

    // // Check if all values are '128' (which is digital silence)
    // let isSilent = true;
    // for (let i = 0; i < nodes.dataArray.length; i++) {
    //     if (nodes.dataArray[i] !== 128) {
    //         isSilent = false;
    //         break;
    //     }
    // }

    // log(`[${audioContext.state}] Set spatial audio for ${participantIdentity} : pan=${nodes.pannerNode.pan.value}, volume=${nodes.gainNode.gain.value}`);
}

/**
 * 
 * @param {LivekitClient.RemoteAudioTrack} remote_audio_track 
 * @param {number} volume 
 */
export function remote_audio_track_set_volume(remote_audio_track, volume) {
    if (remote_audio_track.trackRig) {
        remote_audio_track.trackRig.gainNode.gain.value = Math.max(0, volume);
    } else if (remote_audio_track.audioElement) {
        remote_audio_track.setVolume(volume);
    }
}
