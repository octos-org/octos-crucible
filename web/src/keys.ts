// The platform public key bundled from config/keys.json. GET /pubkey wins
// when the two disagree (key rotation after this page was built).

import keys from "../../config/keys.json";
import type { PublicKeyInfo } from "./crypto";

interface KeysFile {
  current: string;
  keys: { key_id: string; public_key: string }[];
}

export function currentKey(): PublicKeyInfo {
  const f = keys as KeysFile;
  const k = f.keys.find((x) => x.key_id === f.current);
  if (!k) throw new Error("config/keys.json: current key missing");
  return { key_id: k.key_id, public_key: k.public_key };
}
