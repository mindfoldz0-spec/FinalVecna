// Generates the IVR voices ONCE with Gnani (Timbre v3.0) and stores them in
// resource/audio — afterwards the app never calls the API again.
//
//   node scripts/generate-voices.mjs            # generate missing files
//   node scripts/generate-voices.mjs --force    # regenerate all
//
// Reads the gnani key from <repo>/.env  (line: gnani:vach_...)
import { readFileSync, writeFileSync, existsSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const OUT = join(ROOT, 'resource', 'audio');
const FORCE = process.argv.includes('--force');

const API_URL = 'https://api.vachana.ai/api/v1/tts/inference';

function loadGnaniKey() {
  const envPath = join(ROOT, '.env');
  const raw = readFileSync(envPath, 'utf8');
  for (const line of raw.split('\n')) {
    const m = line.match(/^\s*gnani\s*:\s*(\S+)\s*$/i);
    if (m) return m[1];
  }
  throw new Error('no "gnani:" key found in .env');
}

// ---- Scripts -------------------------------------------------------------
// Timbre v3.0 supports inline audio tags (<pauses>, <slow>, ...) for a
// natural, human delivery. Voice: Shivani — warm, patient, empathy-first
// inbound-support persona. Speed 0.95 = calm and deliberate.
const GREETING = {
  file: 'greeting.mp3',
  text: 'Namaste, and welcome to the Mayuresh Cybercrime Bureau. <pauses> ' +
        'An officer will connect with you shortly. <pauses> ' +
        'Please speak your message after the beep.',
  voice: 'Shivani',
  speed: 0.95,
};

// Played right after the caller's message is captured (silence detected).
const CONFIRMATION = {
  file: 'confirmation.mp3',
  text: 'Your message has been recorded. <pauses> Please hold while an officer connects with you.',
  voice: 'Shivani',
  speed: 1.0,
};

async function synth(key, { file, text, voice, speed }) {
  const dest = join(OUT, file);
  if (existsSync(dest) && !FORCE) {
    console.log(`  [cached] ${file}`);
    return;
  }
  console.log(`  [gnani timbre-v3.0 / ${voice}] ${file}: "${text.slice(0, 60)}..."`);
  const res = await fetch(API_URL, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-API-Key-ID': key },
    body: JSON.stringify({
      text,
      voice,
      model: 'timbre-v3.0',
      language: 'en-IN',
      speed,
      audio_config: { sample_rate: 24000, num_channels: 1, container: 'mp3', bitrate: '96k' },
    }),
  });
  if (!res.ok) {
    const body = await res.text().catch(() => '');
    throw new Error(`gnani ${res.status}: ${body.slice(0, 300)}`);
  }
  const buf = Buffer.from(await res.arrayBuffer());
  if (buf.length < 1000) throw new Error(`suspiciously small audio (${buf.length} bytes)`);
  writeFileSync(dest, buf);
  console.log(`  [saved] ${file} (${(buf.length / 1024).toFixed(1)} kB)`);
}

mkdirSync(OUT, { recursive: true });
const key = loadGnaniKey();
console.log(`gnani key loaded (${key.slice(0, 10)}…)`);
for (const job of [GREETING, CONFIRMATION]) {
  await synth(key, job);
}
console.log('done — voices are stored in resource/audio and served at /audio/<file>');
