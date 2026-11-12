// Plays the remote desktop's audio, received through the `audioPlayback` extension, with Web Audio.
//
// Each wave is queued to start where the one before it ends. A browser starts an AudioContext
// suspended until the page sees a click or a key press, so playback starts at the first one.

// How far ahead a wave is queued when playback starts or has fallen behind, in seconds: room for
// the next wave to arrive before this one ends.
const LEAD = 0.08;

// Waves are dropped while more than this many seconds are queued, so latency cannot grow without
// bound (as it would while the context is still suspended).
const MAX_QUEUED = 0.5;

const GESTURES = ['pointerdown', 'keydown'] as const;

type AudioMessage =
    | { type: 'wave'; sampleRate: number; channels: number; data: Uint8Array }
    | { type: 'volume'; left: number; right: number }
    | { type: 'close' };

export interface WebAudioPlayer {
    // The callback to pass to `audioPlayback`.
    handle: (message: unknown) => void;
    // Stops playback and releases the AudioContext.
    close: () => void;
}

// Splits interleaved 16-bit little-endian PCM into one plane of samples in [-1, 1) per channel.
export function pcmToPlanes(data: Uint8Array, channels: number): Float32Array[] {
    const frames = Math.floor(data.byteLength / (2 * channels));
    const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
    const planes = Array.from({ length: channels }, () => new Float32Array(frames));
    for (let frame = 0; frame < frames; frame++) {
        for (let channel = 0; channel < channels; channel++) {
            planes[channel][frame] = view.getInt16((frame * channels + channel) * 2, true) / 32768;
        }
    }
    return planes;
}

export function createWebAudioPlayer(): WebAudioPlayer {
    let ctx: AudioContext | null = null;
    let next = 0;

    const resume = () => {
        void ctx?.resume();
    };

    // Created at the first wave rather than up front, so a session without audio never holds one.
    const context = (): AudioContext => {
        if (ctx == null) {
            ctx = new AudioContext();
            resume();
            for (const event of GESTURES) {
                window.addEventListener(event, resume, true);
            }
        }
        return ctx;
    };

    const play = (sampleRate: number, channels: number, data: Uint8Array) => {
        if (channels === 0) {
            return;
        }
        const planes = pcmToPlanes(data, channels);
        const frames = planes[0].length;
        if (frames === 0) {
            return;
        }
        const ctx = context();
        const now = ctx.currentTime;
        if (next < now) {
            next = now + LEAD;
        }
        if (next - now > MAX_QUEUED) {
            return;
        }

        // Web Audio resamples a buffer whose rate differs from the context's.
        const buffer = ctx.createBuffer(channels, frames, sampleRate);
        planes.forEach((plane, channel) => buffer.copyToChannel(plane, channel));
        const source = ctx.createBufferSource();
        source.buffer = buffer;
        source.connect(ctx.destination);
        source.start(next);
        next += frames / sampleRate;
    };

    return {
        handle: (message: unknown) => {
            const msg = message as AudioMessage;
            switch (msg.type) {
                case 'wave':
                    try {
                        play(msg.sampleRate, msg.channels, msg.data);
                    } catch (e) {
                        console.debug('A wave was not played', e);
                    }
                    break;
                case 'close':
                    // Playback paused or formats are being renegotiated; waves can follow. The next
                    // one starts fresh rather than after whatever was queued.
                    next = 0;
                    break;
                case 'volume':
                    // The levels are logarithmic protocol values, not gains; left at full volume.
                    break;
            }
        },
        close: () => {
            for (const event of GESTURES) {
                window.removeEventListener(event, resume, true);
            }
            void ctx?.close();
            ctx = null;
        },
    };
}
