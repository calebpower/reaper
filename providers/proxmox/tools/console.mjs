#!/usr/bin/env node
//
// Read a guest's serial console through the API.
//
// This exists because of an evening spent inferring, from outside a machine,
// why it would not boot -- when every failure mode in question was printed on
// its console and visible nowhere else. Six hypotheses were tested remotely and
// all six were wrong. A console you can read from a script turns that into an
// ordinary debugging loop.
//
// It lives under providers/proxmox/ rather than tools/ because it is
// hypervisor-specific, and the provider seam guard is right to insist: naming
// this hypervisor anywhere else would be the leak that guard exists to catch.
//
// IMPORTANT, and the reason this needs a credential of its own: **an API token
// cannot open a console.** The termproxy call succeeds with one -- it is an
// ordinary API call and VM.Console covers it -- and then the terminal's own
// authentication step rejects the ticket, because a token-shaped user name
// (`user@realm!tokenid`) is not a user name as far as that code is concerned.
// The websocket opens and is closed a few seconds later with no explanation.
// This is a known limitation rather than anything wrong here.
//
// So this takes a user login instead: --user and --password-file, exchanged for
// a ticket the way the browser does. Give that user VM.Console on the resource
// pool and nothing else; it needs strictly less than the harness token.
//
// Two things are also required of the guest, and neither is this script's
// business to arrange:
//
//   1. the machine has a serial device (`serial0`), and
//   2. the operating system inside it has been told to use one.
//
// Without the first, the API refuses and says so. Without the second the
// connection succeeds and stays silent, which is the more confusing failure --
// so a run that sees no bytes at all says which of the two to check.
//
// Usage:
//   console.mjs --id 9004 [--seconds 180] [--out boot.log] [--send 'fsck -y\n']
//
// Configuration, by flag or environment:
//   --api    REAPER_PVE_API     https://host:8006
//   --node   REAPER_PVE_NODE    the node the guest is on
//   --user   REAPER_PVE_USER    e.g. console@pve
//   --password-file REAPER_PVE_PASSWORD_FILE
//   --insecure                  skip TLS verification, and say so loudly
//
// Exit 0 if the console was reached, 1 otherwise.

import https from 'node:https';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

function parseArgs(argv) {
    const out = { seconds: 180, insecure: false };
    for (let i = 0; i < argv.length; i++) {
        const a = argv[i];
        const need = () => {
            if (i + 1 >= argv.length) die(`${a} needs a value`);
            return argv[++i];
        };
        switch (a) {
            case '--id':       out.id = need(); break;
            case '--seconds':  out.seconds = Number(need()); break;
            case '--out':      out.out = need(); break;
            case '--send':     out.send = need(); break;
            case '--api':      out.api = need(); break;
            case '--node':     out.node = need(); break;
            case '--user':     out.user = need(); break;
            case '--password-file': out.passwordFile = need(); break;
            case '--insecure': out.insecure = true; break;
            case '-h':
            case '--help':     usage(); process.exit(0); break;
            default: die(`unexpected argument ${a}`);
        }
    }
    return out;
}

function usage() {
    // The header of this file is the documentation; printing it keeps the two
    // from drifting apart.
    const self = fs.readFileSync(new URL(import.meta.url), 'utf8');
    const header = self.split('\n').slice(2).filter((l) => l.startsWith('//'));
    console.log(header.map((l) => l.replace(/^\/\/ ?/, '')).join('\n'));
}

function die(message) {
    console.error(`console: ${message}`);
    process.exit(1);
}

const args = parseArgs(process.argv.slice(2));
if (!args.id) die('no --id; which guest?');
if (!/^[0-9]+$/.test(args.id)) die(`--id ${args.id} is not an identifier`);

const api = args.api ?? process.env.REAPER_PVE_API;
const node = args.node ?? process.env.REAPER_PVE_NODE;
if (!api) die('no --api and no REAPER_PVE_API');
if (!node) die('no --node and no REAPER_PVE_NODE');

const user = args.user ?? process.env.REAPER_PVE_USER;
const passwordFile = args.passwordFile ?? process.env.REAPER_PVE_PASSWORD_FILE;

if (!user || !passwordFile) {
    die(
        'a console needs a user login, not an API token.\n' +
        '       A token can call termproxy but cannot authenticate the terminal it\n' +
        '       opens: the ticket check refuses a name of the form user@realm!tokenid,\n' +
        '       so the websocket opens and is closed seconds later saying nothing.\n' +
        '       Pass --user and --password-file. A user with VM.Console on the pool\n' +
        '       and nothing else is enough, and is less privileged than the token.'
    );
}

let password;
try {
    password = fs.readFileSync(passwordFile, 'utf8').trim();
} catch (e) {
    die(`cannot read the password at ${passwordFile}: ${e.message}`);
}

if (args.insecure) {
    // Loud, every invocation, exactly as reaper itself is about this. The
    // warning is the only thing keeping it temporary.
    console.error(
        `console: WARNING: TLS certificate verification is disabled for ${api}. ` +
        `Anyone between here and there can read the API token and rewrite the replies.`
    );
    process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';
}

const base = new URL(api);

// ---------------------------------------------------------------------------
// Getting a ticket
// ---------------------------------------------------------------------------

function post(pathname, form, auth) {
    const body = form ? new URLSearchParams(form).toString() : '';
    return new Promise((resolve, reject) => {
        const req = https.request(
            {
                hostname: base.hostname,
                port: base.port || 443,
                path: pathname,
                method: 'POST',
                headers: {
                    'Content-Type': 'application/x-www-form-urlencoded',
                    'Content-Length': Buffer.byteLength(body),
                    ...(auth ?? {}),
                },
                rejectUnauthorized: !args.insecure,
            },
            (res) => {
                let body = '';
                res.on('data', (d) => (body += d));
                res.on('end', () => {
                    if (res.statusCode !== 200) {
                        reject(new Error(`${res.statusCode} ${res.statusMessage}: ${body.trim()}`));
                        return;
                    }
                    try {
                        resolve(JSON.parse(body).data);
                    } catch (e) {
                        reject(new Error(`unreadable reply: ${e.message}`));
                    }
                });
            }
        );
        req.on('error', reject);
        req.end(body);
    });
}

const guest = `/api2/json/nodes/${node}/qemu/${args.id}`;

// A login ticket, the way the browser gets one. The ticket doubles as the
// cookie for every later call and as the console's own credential.
let login;
try {
    login = await post('/api2/json/access/ticket', { username: user, password });
} catch (e) {
    die(`could not log in as ${user}: ${e.message}`);
}
const cookie = { Cookie: `PVEAuthCookie=${encodeURIComponent(login.ticket)}` };

let proxy;
try {
    proxy = await post(`${guest}/termproxy`, null, cookie);
} catch (e) {
    // The overwhelmingly common cause, and worth naming rather than passing
    // the API's own wording through unexplained.
    die(
        `could not open a terminal on ${args.id}: ${e.message}\n` +
        `       If it says there is no serial device, add one first:\n` +
        `       PUT ${guest}/config  serial0=socket`
    );
}

// ---------------------------------------------------------------------------
// The console itself
// ---------------------------------------------------------------------------

const wsUrl =
    `wss://${base.host}${guest}/vncwebsocket` +
    `?port=${encodeURIComponent(proxy.port)}` +
    `&vncticket=${encodeURIComponent(proxy.ticket)}`;

// A custom header on the handshake is not in the WHATWG WebSocket API, and
// Node's implementation allows it. That is the whole reason this script is
// forty lines rather than a hand-rolled handshake and frame reader.
//
// `binary` is not decoration. The server answers the handshake with that
// subprotocol, and a client that did not offer one must treat the connection as
// failed -- which it does, reporting a transport error that reads like the host
// refused the connection. The handshake was fine all along.
if (process.env.CONSOLE_DEBUG) console.error(`console: url ${wsUrl}`);
const ws = new WebSocket(wsUrl, {
    protocols: ['binary'],
    headers: cookie,
});
ws.binaryType = 'arraybuffer';

const sink = args.out ? fs.createWriteStream(args.out, { flags: 'a' }) : null;
const decoder = new TextDecoder();
let bytes = 0;
let keepalive;

let stopped = false;

function stop(code) {
    // Both the error and close handlers reach here for one failure, and a
    // report printed twice reads like two faults.
    if (stopped) return;
    stopped = true;
    clearInterval(keepalive);
    try { ws.close(); } catch { /* already closing */ }
    if (sink) sink.end();

    if (bytes === 0) {
        console.error(
            `\nconsole: connected, and the guest sent nothing in ${args.seconds}s.\n` +
            `       That usually means the machine has a serial device but the system\n` +
            `       inside it was never told to use one. On this platform that is a\n` +
            `       loader setting; see the guest's runbook.`
        );
    } else {
        console.error(`\nconsole: ${bytes} bytes${args.out ? ` -> ${args.out}` : ''}`);
    }
    process.exit(code);
}

ws.addEventListener('open', () => {
    // The terminal protocol: authenticate, then input is length-prefixed and
    // a bare "2" is a keepalive.
    ws.send(`${proxy.user}:${proxy.ticket}\n`);

    keepalive = setInterval(() => {
        if (ws.readyState === WebSocket.OPEN) ws.send('2');
    }, 15_000);

    if (args.send) {
        // Interpreted so a caller can write \n rather than a literal newline.
        const text = args.send.replace(/\\n/g, '\n').replace(/\\t/g, '\t');
        setTimeout(() => ws.send(`0:${Buffer.byteLength(text)}:${text}`), 1_000);
    }

    console.error(`console: attached to ${args.id}, reading for ${args.seconds}s`);
});

ws.addEventListener('message', (event) => {
    const text =
        typeof event.data === 'string'
            ? event.data
            : decoder.decode(new Uint8Array(event.data));

    // The server answers the auth line with OK before any guest output. It is
    // protocol rather than console content, so it is not counted or recorded.
    if (bytes === 0 && text.trim() === 'OK') return;

    bytes += text.length;
    process.stdout.write(text);
    if (sink) sink.write(text);
});

ws.addEventListener('error', (event) => {
    // Report the cause rather than the event. A bare "websocket error" sends
    // the reader looking in the wrong place, and the usual cause here is
    // mundane: a stopped guest has no process to attach a terminal to.
    if (process.env.CONSOLE_DEBUG) console.error('console: raw error', event.error ?? event);
    const cause = event.error?.cause?.message ?? event.error?.message ?? event.message;
    console.error(`console: could not attach: ${cause || 'connection refused'}`);
    console.error(`       A guest that is not running has no terminal. Check it is started.`);
    stop(1);
});

ws.addEventListener('close', (event) => {
    console.error(`console: closed (${event.code})`);
    stop(bytes > 0 ? 0 : 1);
});

setTimeout(() => stop(bytes > 0 ? 0 : 1), args.seconds * 1_000);
