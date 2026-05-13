const {
  makeWASocket,
  useMultiFileAuthState,
  DisconnectReason,
  makeInMemoryStore,
  fetchLatestBaileysVersion,
  Browsers,
  proto,
} = require("@whiskeysockets/baileys");
const express = require("express");
const QRCode = require("qrcode");
const path = require("path");

// ── Config ──
const PORT = process.env.BRIDGE_PORT || 3469;
const AUTH_DIR = process.env.BRIDGE_AUTH_DIR || path.join(__dirname, "auth");
const BRIDGE_TOKEN = process.env.BRIDGE_TOKEN || "goblin-whatsapp-bridge";

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
    browser: Browsers.macOS("Desktop"),
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

  // ── Incoming messages ──
  sock.ev.on("messages.upsert", (m) => {
    for (const msg of m.messages) {
      if (msg.key.fromMe) continue;

      const sender = msg.key.remoteJid || msg.key.participant;
      let text = "";
      if (msg.message?.conversation) {
        text = msg.message.conversation;
      } else if (msg.message?.extendedTextMessage?.text) {
        text = msg.message.extendedTextMessage.text;
      } else if (msg.message?.imageMessage?.caption) {
        text = `[image] ${msg.message.imageMessage.caption}`;
      } else if (msg.message?.videoMessage?.caption) {
        text = `[video] ${msg.message.videoMessage.caption}`;
      } else {
        text = `[media: ${Object.keys(msg.message || {}).join(", ")}]`;
      }

      if (!text) continue;

      messageQueue.push({
        id: msg.key.id,
        from: sender,
        text,
        timestamp: msg.messageTimestamp
          ? Number(msg.messageTimestamp) * 1000
          : Date.now(),
      });

      if (messageQueue.length > MAX_QUEUE) {
        messageQueue.shift();
      }
    }
  });
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
