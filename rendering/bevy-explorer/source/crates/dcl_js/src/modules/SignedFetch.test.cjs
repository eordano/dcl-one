const test = require('node:test');
const assert = require('node:assert/strict');
const api = require('./SignedFetch.js');
const metadata = JSON.stringify({ hashPayload: 'a'.repeat(64) });

test('signedFetch captures request and protects identity headers', async () => {
  let resolveSign, signed, sent;
  global.Deno = { core: { ops: { op_signed_fetch_headers: (...args) => { signed = args; return new Promise(r => resolveSign = r); } } } };
  global.fetch = async (url, init) => { sent = {url, init}; return {ok:true,status:200,statusText:'OK',headers:{},text:async()=>''}; };
  const input = { url: 'https://example.org/Path', init: { method:'POST', body:'before', headers: {'X-Identity-Metadata':'forged', 'x-authoritative-scope':'forged', accept:'text/plain'} } };
  const pending = api.signedFetch(input);
  input.url = 'https://evil.example/'; input.init.method = 'DELETE'; input.init.body = 'after'; input.init.headers.accept = 'changed';
  resolveSign([['x-identity-metadata',metadata],['x-identity-auth-chain-0','chain']]);
  await pending;
  assert.deepEqual(signed, ['https://example.org/Path','POST','before']);
  assert.equal(sent.url, 'https://example.org/Path'); assert.equal(sent.init.method, 'POST'); assert.equal(sent.init.body, 'before');
  const headers = new Headers(sent.init.headers); assert.equal(headers.get('x-identity-metadata'),metadata); assert.equal(headers.get('x-authoritative-scope'),null); assert.equal(headers.get('accept'),'text/plain');
});

test('getHeaders passes present empty body through', async () => {
  let args;
  global.Deno = {core:{ops:{op_signed_fetch_headers: async (...a) => { args=a; return [['x-identity-metadata',metadata]]; }}}};
  await api.getHeaders({url:'https://example.org/', init:{body:''}});
  assert.deepEqual(args, ['https://example.org/', undefined, '']);
});

test('reserved headers are removed for object, pairs and Headers input', async () => {
  for (const headers of [
    {'X-Identity-Auth-Chain-0':'bad', 'X-Identity-Metadata':'bad', 'X-Authoritative-Scope':'bad', accept:'ok'},
    [['X-Identity-Auth-Chain-0','bad'], ['X-Identity-Metadata','bad'], ['X-Authoritative-Scope','bad'], ['accept','ok']],
    new Headers([['X-Identity-Auth-Chain-0','bad'], ['X-Identity-Metadata','bad'], ['X-Authoritative-Scope','bad'], ['accept','ok']]),
  ]) {
    global.Deno = {core:{ops:{op_signed_fetch_headers: async () => [['x-identity-metadata','engine']]}}};
    let sent;
    global.fetch = async (_, init) => { sent = new Headers(init.headers); return {ok:true,status:200,statusText:'OK',headers:{},text:async()=>''}; };
    await api.signedFetch({url:'https://example.org/', init:{headers}});
    assert.equal(sent.get('x-identity-auth-chain-0'), null);
    assert.equal(sent.get('x-authoritative-scope'), null);
    assert.equal(sent.get('x-identity-metadata'), 'engine');
    assert.equal(sent.get('accept'), 'ok');
  }
});

test('works in a scene global without a Headers constructor', async () => {
  const vm = require('node:vm');
  const fs = require('node:fs');
  let sent;
  const context = { module:{exports:{}}, Deno:{core:{ops:{op_signed_fetch_headers:async()=>[['x-identity-metadata','engine']]}}},
    fetch:async (url,init) => { sent = {url,init}; return {ok:true,status:200,statusText:'OK',headers:{},text:async()=>''}; } };
  vm.runInNewContext(fs.readFileSync(require.resolve('./SignedFetch.js'), 'utf8'), context);
  await context.module.exports.signedFetch({url:'https://example.org/',init:{headers:{'X-Identity-Metadata':'bad',accept:'ok'}}});
  assert.equal(sent.init.headers.find(([key]) => key.toLowerCase() === 'x-identity-metadata')[1], 'engine');
});

test('rejects mutable non-string request fields before signing', async () => {
  global.Deno = {core:{ops:{op_signed_fetch_headers: async () => { throw new Error('must not sign'); }}}};
  for (const input of [
    {url:{toString:()=> 'https://example.org/'}},
    {url:'https://example.org/',init:{method:{toString:()=> 'POST'}}},
    {url:'https://example.org/',init:{body:{toString:()=> '{}'}}},
    {url:'https://example.org/',init:{headers:{accept:{toString:()=> 'text/plain'}}}},
  ]) await assert.rejects(api.signedFetch(input), TypeError);
});

test('null method and body are captured as absent', async () => {
  let args, sent;
  global.Deno = {core:{ops:{op_signed_fetch_headers: async (...a) => {args=a; return [];}}}};
  global.fetch = async (_, init) => {sent=init; return {ok:true,status:200,statusText:'OK',headers:{},text:async()=>''};};
  await api.signedFetch({url:'https://example.org/',init:{method:null,body:null}});
  assert.deepEqual(args,['https://example.org/',undefined,undefined]);
  assert.equal(sent.method,undefined); assert.equal(sent.body,undefined);
  await api.getHeaders({url:'https://example.org/',init:{method:null,body:null}});
  assert.deepEqual(args,['https://example.org/',undefined,undefined]);
});

test('bodyful calls reject legacy or malformed metadata without issuing fetch', async () => {
  for (const metadata of ['{}', '{"hashPayload":"bad"}', 'not-json']) {
    let fetched = false;
    global.Deno = {core:{ops:{op_signed_fetch_headers: async () => [['x-identity-metadata', metadata]]}}};
    global.fetch = async () => {fetched=true; throw new Error('unexpected fetch');};
    await assert.rejects(api.signedFetch({url:'https://example.org/',init:{body:''}}));
    await assert.rejects(api.getHeaders({url:'https://example.org/',init:{body:'{}'}}));
    assert.equal(fetched,false);
  }
});

test('inherited metadata hash cannot satisfy body binding', async () => {
  Object.prototype.hashPayload = 'a'.repeat(64);
  try {
    global.Deno = {core:{ops:{op_signed_fetch_headers: async () => [['x-identity-metadata','{}']]}}};
    await assert.rejects(api.getHeaders({url:'https://example.org/',init:{body:''}}));
  } finally { delete Object.prototype.hashPayload; }
});

test('getHeaders preserves caller headers and lets signer headers win without case duplicates', async () => {
  for (const body of [undefined, '', '{}']) {
    for (const headers of [
      {accept:'application/json', 'Content-Type':'caller', 'X-Identity-Metadata':'forged', 'X-Identity-Auth-Chain-0':'forged', 'X-Authoritative-Scope':'forged'},
      [['accept','application/json'], ['Content-Type','caller'], ['X-Identity-Metadata','forged'], ['X-Identity-Auth-Chain-0','forged'], ['X-Authoritative-Scope','forged']],
      new Headers({accept:'application/json', 'Content-Type':'caller', 'X-Identity-Metadata':'forged', 'X-Identity-Auth-Chain-0':'forged', 'X-Authoritative-Scope':'forged'}),
    ]) {
      const before = [...new Headers(headers).entries()];
      global.Deno = {core:{ops:{op_signed_fetch_headers: async () => [
        ['x-identity-metadata',metadata], ['x-identity-auth-chain-0','engine-chain'], ['content-type','signed'],
      ]}}};
      const result = await api.getHeaders({url:'https://example.org/',init:{body,headers}});
      const returned = new Headers(result.headers);
      assert.equal(returned.get('accept'),'application/json');
      assert.equal(returned.get('content-type'),'signed');
      assert.equal(returned.get('x-identity-metadata'),metadata);
      assert.equal(returned.get('x-identity-auth-chain-0'),'engine-chain');
      assert.equal(returned.get('x-authoritative-scope'),null);
      const names = Object.keys(result.headers).map(name => name.toLowerCase());
      assert.equal(new Set(names).size,names.length);
      assert.deepEqual([...new Headers(headers).entries()],before);
    }
  }
});

test('getHeaders snapshots caller headers, URL, method and body before awaiting the signer', async () => {
  let resolveSign, signed;
  global.Deno = {core:{ops:{op_signed_fetch_headers: (...args) => {
    signed = args;
    return new Promise(resolve => resolveSign = resolve);
  }}}};
  const input = {url:'https://example.org/Path',init:{method:'POST',body:'before',headers:{accept:'application/json'}}};
  const pending = api.getHeaders(input);
  input.url = 'https://changed.example/';
  input.init.method = 'DELETE';
  input.init.body = 'after';
  input.init.headers.accept = 'text/plain';
  resolveSign([['x-identity-metadata',metadata]]);
  const result = await pending;
  assert.deepEqual(signed,['https://example.org/Path','POST','before']);
  assert.equal(result.headers.accept,'application/json');
  assert.equal(result.headers['x-identity-metadata'],metadata);
  assert.equal(input.init.headers.accept,'text/plain');
});
