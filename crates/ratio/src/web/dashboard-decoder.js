"use strict";

const CHUNK = 128 * 1024;
const wasm = WebAssembly.instantiateStreaming(fetch("/dashboard-decoder.wasm")).then(({ instance }) => {
  instance.exports._initialize?.();
  return instance.exports;
});

wasm.then(() => postMessage({ ready: true }), () => postMessage({ ready: false }));

let queue = Promise.resolve();
self.onmessage = ({ data }) => {
  queue = queue.then(() => decode(data));
};

async function decode({ serial, frame, decoded, dictionary, id }) {
  try {
    const codec = await wasm;
    const input = new Uint8Array(frame, 13);
    if (decoded < 1 || decoded > CHUNK || input.length > CHUNK + 1024) {
      throw new Error("Invalid dashboard frame size");
    }
    new Uint8Array(codec.memory.buffer, codec.ratio_input(), input.length).set(input);
    if (!codec.ratio_decode(input.length, decoded, id)) throw new Error("Invalid Zstandard frame");
    const output = new Uint8Array(codec.memory.buffer, codec.ratio_output(), decoded);
    if (dictionary) {
      const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", output));
      const hex = Array.from(hash, byte => byte.toString(16).padStart(2, "0")).join("");
      if (hex !== dictionary.sha256 || !codec.ratio_set_dictionary(decoded, dictionary.id)) {
        throw new Error("Invalid dashboard dictionary");
      }
      postMessage({ serial });
      return;
    }
    const body = output.slice();
    postMessage({ serial, body }, [body.buffer]);
  } catch (error) {
    postMessage({ serial, error: error.message });
  }
}
