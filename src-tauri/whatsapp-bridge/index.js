const {
  makeWASocket,
  useMultiFileAuthState,
  DisconnectReason,
  fetchLatestBaileysVersion,
  Browsers,
} = require("@whiskeysockets/baileys");
const express = require("express");
const QRCode = require("qrcode");
const path = require("path");

// ── Config ──
const PORT = process.env.BRIDGE_PORT || 3469;
const AUTH_DIR = process.env.BRIDGE_AUTH_DIR || path.join(
  process.env.HOME || require("os").homedir(),
  ".goblin", "whatsapp-auth"
);
const BRIDGE_TOKEN = process.env.BRIDGE_TOKEN || "goblin-whatsapp-bridge";

// Verbose logging is OFF by default — every incoming WhatsApp message would
// otherwise spam stdout (and worse, leak message content via the QUEUED log).
// Set WA_BRIDGE_VERBOSE=1 to re-enable for debugging.
const VERBOSE = process.env.WA_BRIDGE_VERBOSE === "1";
const vlog = (...args) => { if (VERBOSE) console.log(...args); };

// ── State ──
let sock = null;
let qrCode = null;
let connectionStatus = "disconnected";
let connectionError = null;
const messageQueue = [];
const MAX_QUEUE = 200;

// ── Express ──
const app = express();
app.use(express.json());

// Auth middleware
function auth(req, res, next) {
  const token = req.headers["x-bridge-token"] || req.query.token;
  if (token !== BRIDGE_TOKEN) {
    return res.status(401).json({ error: "unauthorized" });
  }
  next();
}

app.use(auth);

// ── Endpoints ──

// QR code (returns PNG base64)
app.get("/qr", (_req, res) => {
  if (!qrCode) {
    return res.json({ qr: null, status: connectionStatus });
  }
  QRCode.toBuffer(qrCode, { type: "png", width: 300, margin: 2 })
    .then((buf) => {
      res.json({
        qr: `data:image/png;base64,${buf.toString("base64")}`,
        status: connectionStatus,
      });
    })
    .catch(() => res.json({ qr: null, status: connectionStatus, error: "qr generation failed" }));
});

// Status
app.get("/status", (_req, res) => {
  res.json({
    status: connectionStatus,
    error: connectionError,
    user: sock?.user
      ? { jid: sock.user.id, name: sock.user.name }
      : null,
  });
});

// Send message
app.post("/send", (req, res) => {
  const { jid, text } = req.body;
  if (!sock || connectionStatus !== "connected") {
    return res.status(503).json({ error: "not connected" });
  }
  if (!jid || !text) {
    return res.status(400).json({ error: "jid and text required" });
  }

  const target = jid.includes("@") ? jid : `${jid}@s.whatsapp.net`;

  sock
    .sendMessage(target, { text })
    .then((msg) => res.json({ success: true, id: msg?.key?.id }))
    .catch((err) => res.status(500).json({ error: String(err) }));
});

// Poll messages
app.get("/messages", (_req, res) => {
  const msgs = messageQueue.splice(0, messageQueue.length);
  res.json({ messages: msgs });
});

// Health
app.get("/health", (_req, res) => {
  res.json({ ok: true });
});

// Profile picture proxy. Baileys returns a temporary CDN URL; we fetch
// the bytes once and cache as a data URL for 24h so the frontend can
// render <img src> without CORS issues. Returns { photo: null } on
// missing/private profiles so the UI can fall back to initials.
const PHOTO_TTL_MS = 24 * 60 * 60 * 1000;
const photoCache = new Map(); // jid -> { dataUrl: string|null, fetchedAt: number }
app.get("/profile-picture/:jid", async (req, res) => {
  const jid = req.params.jid;
  if (!sock || connectionStatus !== "connected") {
    return res.status(503).json({ error: "not connected" });
  }
  const cached = photoCache.get(jid);
  if (cached && Date.now() - cached.fetchedAt < PHOTO_TTL_MS) {
    return res.json({ photo: cached.dataUrl });
  }
  try {
    const url = await sock.profilePictureUrl(jid, "image");
    if (!url) {
      photoCache.set(jid, { dataUrl: null, fetchedAt: Date.now() });
      return res.json({ photo: null });
    }
    const r = await fetch(url);
    if (!r.ok) {
      photoCache.set(jid, { dataUrl: null, fetchedAt: Date.now() });
      return res.json({ photo: null });
    }
    const buf = Buffer.from(await r.arrayBuffer());
    const dataUrl = `data:image/jpeg;base64,${buf.toString("base64")}`;
    photoCache.set(jid, { dataUrl, fetchedAt: Date.now() });
    res.json({ photo: dataUrl });
  } catch (_e) {
    // 404 / private profile / blocked, etc — cache null so we don't retry-loop.
    photoCache.set(jid, { dataUrl: null, fetchedAt: Date.now() });
    res.json({ photo: null });
  }
});

// Locally-maintained contact name map. Populated from:
//   1. contacts.upsert / contacts.update Baileys events (saved address-book entries)
//   2. msg.pushName on every incoming message (sender's "display name on WA")
// Baileys 7.x does not ship a built-in store, so we keep our own. Falls
// back to null when no name is known — frontend then shows the JID.
const contactNames = new Map(); // jid -> name

function setContactName(jid, name) {
  if (!jid || !name) return;
  const existing = contactNames.get(jid);
  // Prefer the first non-null name we learn; do not overwrite a saved
  // contact name (from contacts.upsert) with a push_name later.
  if (!existing) contactNames.set(jid, name);
}

app.get("/contacts", (_req, res) => {
  const out = [];
  for (const [jid, name] of contactNames) {
    out.push({ jid, name });
  }
  res.json({ contacts: out });
});

// Pairing code (8-digit, phone-number based, QR alternative)
app.post("/pair", async (req, res) => {
  const { phone } = req.body || {};
  if (!sock) {
    return res.status(503).json({ error: "socket not ready" });
  }
  if (connectionStatus === "connected") {
    return res.status(400).json({ error: "already connected" });
  }
  if (!phone || !/^\d{8,15}$/.test(String(phone))) {
    return res.status(400).json({ error: "phone must be digits only with country code, e.g. 905551234567" });
  }
  try {
    const code = await sock.requestPairingCode(String(phone));
    // Never log the pairing code or phone — the secret is returned via
    // HTTP response and shown in the app UI. Stdout would leak it to the
    // terminal scrollback / any log capture.
    console.log(`[bridge] Pairing code issued (length=${code.length})`);
    res.json({ code, phone: String(phone) });
  } catch (err) {
    res.status(500).json({ error: String(err) });
  }
});

// ── Baileys Connection ──

async function start() {
  const { state, saveCreds } = await useMultiFileAuthState(AUTH_DIR);
  let waVersion;
  try {
    const { version, isLatest } = await fetchLatestBaileysVersion();
    waVersion = version;
    console.log(`[bridge] WA web version ${version.join(".")} (latest=${isLatest})`);
  } catch (e) {
    console.log(`[bridge] fetchLatestBaileysVersion failed, using baileys default: ${e.message}`);
  }

  sock = makeWASocket({
    auth: state,
    ...(waVersion ? { version: waVersion } : {}),
    printQRInTerminal: false,
    browser: Browsers.ubuntu("Chrome"),
    markOnlineOnConnect: false,
    syncFullHistory: false,
    shouldSyncHistoryMessage: () => false,
    getMessage: async (_key) => {
      return { conversation: "" };
    },
  });

  sock.ev.on("connection.update", (update) => {
    const { connection, lastDisconnect, qr } = update;

    if (qr) {
      qrCode = qr;
      connectionStatus = "qr";
      connectionError = null;
      console.log("[bridge] QR code received");
    }

    if (connection === "open") {
      connectionStatus = "connected";
      qrCode = null;
      connectionError = null;
      console.log("[bridge] Connected to WhatsApp");
    }

    if (connection === "close") {
      const reason = lastDisconnect?.error?.output?.statusCode;
      const shouldReconnect = reason !== DisconnectReason.loggedOut;

      if (reason === DisconnectReason.loggedOut) {
        connectionStatus = "logged_out";
        connectionError = "Logged out. Re-authentication required.";
        console.log("[bridge] Logged out");
      } else if (shouldReconnect) {
        connectionStatus = "reconnecting";
        connectionError = `Connection closed (${reason}). Reconnecting...`;
        console.log(`[bridge] Reconnecting (reason: ${reason})`);
        start();
      } else {
        connectionStatus = "disconnected";
        connectionError = `Connection closed (${reason})`;
      }
    }
  });

  sock.ev.on("creds.update", saveCreds);

  // ── Contact names: address-book entries from WhatsApp ──
  const ingestContacts = (list) => {
    for (const c of list || []) {
      const name = c?.notify || c?.verifiedName || c?.name;
      if (c?.id && name) setContactName(c.id, name);
    }
  };
  sock.ev.on("contacts.upsert", ingestContacts);
  sock.ev.on("contacts.update", ingestContacts);

  // ── Incoming messages ──
  sock.ev.on("messages.upsert", (m) => {
    vlog(`[bridge] messages.upsert: ${m.messages.length} message(s), type=${m.type}`);
    for (const msg of m.messages) {
      vlog(`[bridge] msg keys: ${Object.keys(msg.message || {}).join(",")} fromMe=${msg.key.fromMe}`);
      if (msg.key.fromMe) continue;
      if (!msg.message) continue;

      // ── Filter junk message types ──
      // These never appear as standalone chat items in the WA UI either —
      // they are protocol artefacts (delivery receipts, reactions on other
      // messages, system "kept" notifications, sender-key handshakes).
      const SKIP_TYPES = [
        "protocolMessage",
        "messageContextInfo",
        "senderKeyDistributionMessage",
        "reactionMessage",
        "keepInChatMessage",
        "ephemeralMessage",
        "pollUpdateMessage",
      ];
      const keys = Object.keys(msg.message);
      if (keys.every((k) => SKIP_TYPES.includes(k))) continue;

      // Strip skip-types from the visible key list so the label fallback
      // ("[media: ...]") never echoes them.
      const visibleKeys = keys.filter((k) => !SKIP_TYPES.includes(k));

      const sender = msg.key.remoteJid || msg.key.participant;
      if (msg.pushName) setContactName(sender, msg.pushName);
      const text = formatMessageText(msg.message, visibleKeys);
      if (!text) continue;

      const entry = {
        id: msg.key.id,
        from: sender,
        push_name: msg.pushName || null,
        text,
        timestamp: msg.messageTimestamp
          ? Number(msg.messageTimestamp) * 1000
          : Date.now(),
      };
      vlog(`[bridge] QUEUED: from=${sender} text="${text}"`);
      messageQueue.push(entry);

      if (messageQueue.length > MAX_QUEUE) {
        messageQueue.shift();
      }
    }
  });
}

// ── Message-content formatter ──
// Turn a Baileys message object into a single line of display text. Plain
// chat → just the text. Media → emoji-prefixed label + optional caption.
// Unknown types → "📎 Mesaj" so the UI never shows internal struct names.
function formatMessageText(message, visibleKeys) {
  if (message?.conversation) return message.conversation;
  if (message?.extendedTextMessage?.text) return message.extendedTextMessage.text;

  const cap = (m) => (m?.caption ? `: ${m.caption}` : "");
  if (message?.imageMessage)    return `📷 Foto${cap(message.imageMessage)}`;
  if (message?.videoMessage)    return `🎥 Video${cap(message.videoMessage)}`;
  if (message?.audioMessage)    return message.audioMessage.ptt ? "🎙️ Sesli mesaj" : "🔊 Ses";
  if (message?.documentMessage) {
    const name = message.documentMessage.fileName || "belge";
    return `📄 ${name}`;
  }
  if (message?.stickerMessage)  return "🪧 Çıkartma";
  if (message?.contactMessage)  return "👤 Kişi kartı";
  if (message?.locationMessage || message?.liveLocationMessage) return "📍 Konum";
  if (message?.pollCreationMessage || message?.pollCreationMessageV3) return "📊 Anket";

  // Unknown / new Baileys type → generic placeholder, never the internal name.
  return visibleKeys.length > 0 ? "📎 Mesaj" : "";
}

start().catch((err) => {
  console.error("[bridge] Fatal:", err);
  connectionStatus = "error";
  connectionError = String(err);
});

// ── Start HTTP server ──
app.listen(PORT, "127.0.0.1", () => {
  console.log(`[bridge] WhatsApp bridge listening on http://127.0.0.1:${PORT}`);
});

// Graceful shutdown
process.on("SIGTERM", () => {
  console.log("[bridge] Shutting down");
  if (sock) sock.end();
  process.exit(0);
});
process.on("SIGINT", () => {
  console.log("[bridge] Shutting down");
  if (sock) sock.end();
  process.exit(0);
});
