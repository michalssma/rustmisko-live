'use strict';
// One-time setup: creates data/dashboard.secret with bcrypt PIN hash + JWT secret
// Usage: node setup.js
//        node setup.js --reset-pin

const bcrypt = require('bcryptjs');
const crypto = require('crypto');
const fs   = require('fs');
const path = require('path');
const readline = require('readline');

const SECRET_FILE = path.join(__dirname, '../data/dashboard.secret');

function prompt(question, hidden = false) {
  return new Promise(resolve => {
    const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
    if (hidden && process.stdin.isTTY) process.stdin.setRawMode(true);
    rl.question(question, answer => {
      if (hidden && process.stdin.isTTY) { process.stdin.setRawMode(false); console.log(); }
      rl.close();
      resolve(answer);
    });
  });
}

async function main() {
  const args = process.argv.slice(2);
  const reset = args.includes('--reset-pin');

  if (!reset && fs.existsSync(SECRET_FILE)) {
    console.log('PIN already configured. Use --reset-pin to change it.');
    console.log('Start dashboard: node dashboard/server.js');
    return;
  }

  console.log('=== RustMiskoLive Dashboard Setup ===\n');

  const pin = await prompt('Enter 6-digit PIN: ');
  if (!/^\d{6}$/.test(pin)) {
    console.error('\nERROR: PIN must be exactly 6 digits (0-9).');
    process.exit(1);
  }

  const confirm = await prompt('Confirm PIN: ');
  if (pin !== confirm) {
    console.error('\nERROR: PINs do not match.');
    process.exit(1);
  }

  console.log('\nHashing PIN (this takes a moment)...');
  const hash = await bcrypt.hash(pin, 12);
  const jwt_secret = crypto.randomBytes(32).toString('hex');

  const secret = { hash, jwt_secret, created: new Date().toISOString() };
  fs.mkdirSync(path.dirname(SECRET_FILE), { recursive: true });
  fs.writeFileSync(SECRET_FILE, JSON.stringify(secret), { mode: 0o600 });

  console.log(`\nDone! Secret saved to ${SECRET_FILE}`);
  console.log('Start dashboard: node dashboard/server.js');
}

main().catch(e => { console.error(e.message); process.exit(1); });
