function snapshot(body) {
    const url = body.url;
    const init = { ...body.init };
    if (init.method === null) init.method = undefined;
    if (init.body === null) init.body = undefined;
    if (typeof url !== "string" || (init.method !== undefined && typeof init.method !== "string") ||
        (init.body !== undefined && typeof init.body !== "string")) {
        throw new TypeError("signed fetch URL, method and body must be strings");
    }
    const headers = [];
    const add = (name, value) => {
        if (typeof name !== "string" || typeof value !== "string") throw new TypeError("signed fetch headers must be strings");
        const lower = name.toLowerCase();
        if (!lower.startsWith("x-identity-") && lower !== "x-authoritative-scope") {
            headers.push([name, value]);
        }
    };
    const input = init.headers;
    if (Array.isArray(input)) {
        for (const [name, value] of input) add(name, value);
    } else if (input && typeof input.forEach === "function") {
        input.forEach((value, name) => add(name, value));
    } else if (input) {
        for (const [name, value] of Object.entries(input)) add(name, value);
    }
    init.headers = headers;
    return { url, init };
}

function requireBodyHash(headers, body) {
    if (body === undefined) return;
    const metadata = headers.find(([name]) => name.toLowerCase() === "x-identity-metadata");
    let hash;
    try {
        const parsed = JSON.parse(metadata?.[1]);
        if (Object.prototype.hasOwnProperty.call(parsed, "hashPayload")) hash = parsed.hashPayload;
    } catch (_) { /* invalid or missing */ }
    if (typeof hash !== "string" || !/^[0-9a-f]{64}$/.test(hash)) {
        throw new Error("signed fetch signer did not bind the request body");
    }
}

module.exports.signedFetch = async function (body) {
    const { url, init } = snapshot(body);
    const headers = await Deno.core.ops.op_signed_fetch_headers(url, init.method, init.body);
    requireBodyHash(headers, init.body);
    for (const [name, value] of headers) init.headers.push([name, value]);
    const response = await fetch(url, init);
    return {
        ok: response.ok,
        status: response.status,
        statusText: response.statusText,
        headers: response.headers,
        body: await response.text(),
    };
};

module.exports.getHeaders = async function (body) {
    const { url, init } = snapshot(body);
    const result = await Deno.core.ops.op_signed_fetch_headers(url, init.method, init.body);
    requireBodyHash(result, init.body);
    const headers = new Map();
    for (const [name, value] of [...init.headers, ...result]) {
        headers.set(name.toLowerCase(), [name, value]);
    }
    return { headers: Object.fromEntries(headers.values()) };
};
