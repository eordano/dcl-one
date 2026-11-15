import assert from 'node:assert/strict'
import { describe, it } from 'node:test'
import {
  calculateMultipleHashesADR32,
  calculateMultipleHashesADR32LegacyQmHash,
  hashV0,
  hashV1,
  keccak256Hash,
  prepareADR32Data
} from '../src/index.ts'
import { CHUNK_SIZE_BYTES, hashV1WithLayout } from '../src/layout.ts'

// Byte i is i % 251. The hashes are the ones the hashing packages on npm (1.2.0 and 3.0.4) return.
const patterned = [
  {
    size: 0,
    v0: 'QmdfTbBqBPQ7VNxZEYEj14VmRuZBkqFbiwReogJgS1zR1n',
    v1: 'bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku'
  },
  {
    size: 1,
    v0: 'QmVkqaunyKCw4NKmWXbsGhkr5CotM8vCBKWyxsh8FGZyfr',
    v1: 'bafkreidogqfzz75tpkmjzjke425xqcrmpcib2p5tg44hnbirumdbpl5adu'
  },
  {
    size: 262144,
    v0: 'QmRgPwRd8PpGwUY6g1yqHvqDMKeR3o5obr1fiNj5Nu3XNM',
    v1: 'bafkreibruh455iawsviqslif5c7uurdcfdemh22mtnytyzvnzn75kpejxy'
  },
  {
    size: 262145,
    v0: 'QmVUgDpQtVyPdvzgVPV86s8ya9NCJkF54eYVkuzhZ487SZ',
    v1: 'bafybeiexg2oqkfnj56l7fcmawswqbijt5shq4b5rg6a546uwpkqqzwjioi'
  },
  {
    size: 1048576,
    v0: 'QmV1XQY5kfiCsupnCCD9gjJeWBpvw6f1DhHLmJNmiPGf6Q',
    v1: 'bafybeiedpcapwld4tkgtzwahfofgn4wex5ryysf4se6hwpmlrsh4ntnrau'
  }
]

// `chunkCount` chunks of `chunkSize` bytes, each zero but for its index in the first four bytes.
// The expected hashes are from ipfs-unixfs-importer 15.4.0 (CIDv1, raw leaves, fixed-size chunker, balanced layout).
async function* indexedChunks(chunkCount: number, chunkSize: number): AsyncGenerator<Uint8Array> {
  for (let i = 0; i < chunkCount; i++) {
    const chunk = new Uint8Array(chunkSize)
    chunk[0] = i & 0xff
    chunk[1] = (i >>> 8) & 0xff
    chunk[2] = (i >>> 16) & 0xff
    chunk[3] = (i >>> 24) & 0xff
    yield chunk
  }
}

const twoLevels: Array<[number, string]> = [
  [175, 'bafybeiaqquy5nm2mwvvewo2w3r4xtr3on2i4m2rco3ls7mpxp3y6rge6e4'],
  [200, 'bafybeie6nnyjfmveabfjzy63jdwp2lnsrhcx6vrzallhkadig6f4wcm3le'],
  [348, 'bafybeigrzojtcb5aov37ai75cllm4q6xwdnfddxjbujubmzrmtd2tvedc4'],
  [349, 'bafybeig3mxowaycxr2re73p2ysbii3ghp4phdm3gv43ieitbjxr52fyfza']
]

const smallFanOut: Array<[number, string]> = [
  [4, 'bafybeifv44dksyuml257iebtu2uxurpux7p4wggecgsv7lj5y6ipdllrhi'],
  [9, 'bafybeibcvqxqo6hstepsrwbbiwvzt64owc7277xg27xwwx2yxuurhxfw2y'],
  [10, 'bafybeigm7i2cuaaeqgtn4n5nogxsa2ui2f3crkzs33libqdc2amac2jpge'],
  [27, 'bafybeif6s5jougkfd3zkouc3sazfqpozva3iaf7mdbfjiq6sk5lkdfv4bm'],
  [28, 'bafybeiftg53eyneicsnoiai2xp5fd4up5po3nbxqp5how7jt3qfcwpqf4a'],
  [100, 'bafybeicad7rd3hqvnwsiqtbc6ofpaz2j6eeer5pcw44nl6tjadaame2n6y']
]

describe('hashV0 and hashV1', () => {
  for (const { size, v0, v1 } of patterned) {
    it(`hash ${size} bytes from a buffer and from a stream`, async () => {
      const bytes = Uint8Array.from({ length: size }, (_, i) => i % 251)
      async function* stream(): AsyncGenerator<Uint8Array> {
        for (let at = 0; at < bytes.length; at += 16387) yield bytes.subarray(at, at + 16387)
      }
      assert.equal(await hashV0(bytes), v0)
      assert.equal(await hashV1(bytes), v1)
      assert.equal(await hashV0(stream()), v0)
      assert.equal(await hashV1(stream()), v1)
    })
  }

  for (const [chunkCount, expected] of twoLevels) {
    it(`hashV1 builds the two-level tree of ${chunkCount} chunks`, async () => {
      assert.equal(await hashV1(indexedChunks(chunkCount, CHUNK_SIZE_BYTES)), expected)
    })
  }

  for (const [chunkCount, expected] of smallFanOut) {
    it(`the layout builds the tree of ${chunkCount} chunks with three children per node`, async () => {
      const options = { chunkSize: 16, maxChildrenPerNode: 3 }
      assert.equal(await hashV1WithLayout(indexedChunks(chunkCount, 16), options), expected)

      const buffer = new Uint8Array(chunkCount * 16)
      let at = 0
      for await (const chunk of indexedChunks(chunkCount, 16)) {
        buffer.set(chunk, at)
        at += 16
      }
      assert.equal(await hashV1WithLayout(buffer, options), expected)
    })
  }

  it('hash a stream whose parts are larger than a chunk', async () => {
    const bytes = Uint8Array.from({ length: 1048576 }, (_, i) => i % 251)
    for (const partSize of [262144, 300000, 524288, 1048576]) {
      async function* stream(): AsyncGenerator<Uint8Array> {
        for (let at = 0; at < bytes.length; at += partSize) yield bytes.subarray(at, at + partSize)
      }
      assert.equal(await hashV1(stream()), 'bafybeiedpcapwld4tkgtzwahfofgn4wex5ryysf4se6hwpmlrsh4ntnrau')
    }
  })

  it('hashV0 gives one hash to a long stream and to its bytes', async () => {
    // 17 MiB: the stream is longer than hashV0 collects, so it is hashed as it arrives
    const bytes = new Uint8Array(17 * 1024 * 1024)
    for (let i = 0; i < bytes.length; i += 4099) bytes[i] = i % 253
    async function* stream(): AsyncGenerator<Uint8Array> {
      for (let at = 0; at < bytes.length; at += 65536) yield bytes.subarray(at, at + 65536)
    }
    assert.equal(await hashV0(stream()), await hashV0(bytes))
    assert.equal(await hashV0(bytes), 'QmXT8TcGLEosiuCoptYN1b2tVBMK4F1dbQxRPJyWrk2Yq3')
  })

  it('hash a stream that changes its buffer after each part', async () => {
    const bytes = Uint8Array.from({ length: 262145 }, (_, i) => i % 251)
    async function* stream(): AsyncGenerator<Uint8Array> {
      const shared = new Uint8Array(16387)
      for (let at = 0; at < bytes.length; at += shared.length) {
        const part = bytes.subarray(at, at + shared.length)
        shared.set(part)
        yield shared.subarray(0, part.length)
      }
    }
    assert.equal(await hashV0(stream()), 'QmVUgDpQtVyPdvzgVPV86s8ya9NCJkF54eYVkuzhZ487SZ')
  })

  it('reject a value that is neither bytes nor a stream of bytes', async () => {
    await assert.rejects(hashV0('text' as never), /Invalid value provided to hashV0/)
    await assert.rejects(hashV1('text' as never), /Invalid value provided to hashV1/)
    async function* text(): AsyncGenerator<Uint8Array> {
      yield 'text' as never
    }
    await assert.rejects(hashV0(text()))
    await assert.rejects(hashV1(text()))
  })
})

// The values are the ones the hashing packages on npm (1.1.3, 1.2.0 and 3.0.4) return, except where a comment names a version.
describe('edge inputs', () => {
  const entry = (file: string, hash: string) => ({ file, hash })
  const textOf = (data: Uint8Array): string => new TextDecoder().decode(data)
  const keysOf = (data: Uint8Array): string[] =>
    JSON.parse(textOf(data)).content.map((item: { key: string }) => item.key)

  it('hash the bytes of a text, so each encoding has its own hash', async () => {
    const text = 'Decentraland \u00e9 \u65e5\u672c \u{1f600}\r\nline\n'
    const encoded: Array<[Uint8Array, string, string]> = [
      [
        Buffer.from(text, 'utf8'),
        'QmbmJcQtPZyjn2ghh1oSgApPJVp5c1VFobmmv9raLwYfQS',
        'bafkreighpc42msfbv3tdfn5ve5mulevzlklu5zr3ej2qz32g7m6lirykp4'
      ],
      [
        // with a byte order mark
        Buffer.from('\ufeff' + text, 'utf8'),
        'QmdadNftWojwr9P3PArUn4uKuMDj1Yup6XSZWpZhBHrAmn',
        'bafkreihcopwpmbhpdln26jzq3fcwpjw2vjwzlw24ngt23zs4wo5z5pa6ke'
      ],
      [
        Buffer.from(text, 'utf16le'),
        'QmeCq5LhBhC3Y62rpVVivcXSFdLPWw2PbW1oPW8QEYCqk4',
        'bafkreihlxiiltoelpixqjc6e2u5osd4den425v4vwdgyq4v2flb6knsire'
      ],
      [
        Buffer.from('caf\u00e9', 'latin1'),
        'Qmd5VhPLpGY6YbxJwQBcFkTNm5C5MvWNyKq5yipW75iEvq',
        'bafkreig27vtmbomjmxtirpq7yeuufqe7anionpqgquaxypzdj2l5blojfy'
      ],
      [
        // one composed character
        Buffer.from('\u00e9'),
        'QmTMrVHaBRekBe3N7kLRWZVDjVeTXJ1qE4zf9WjvpVArRq',
        'bafkreicktfkx4qbtynjz3yxlmvdsaf6k2x4vk732ays2bhy4h5xcxju4jq'
      ],
      [
        // the same character as a letter and a combining accent
        Buffer.from('e\u0301'),
        'QmbCWueApqAD9gfBidPjSmSRwGCWVTHWSiar7Kawei8VQ9',
        'bafkreif7cj3hwdzkk2zbsadvxlubnh3fny6orvrvpvfp6gcly3d6ushz6y'
      ],
      [
        Buffer.from('a\nb'),
        'QmWptABvHuuLontbJGHs66T35orsFmoZTZFsu4R4DWmrjZ',
        'bafkreid6dd3tomi3fxb3f4tj3v4ds2ydkhyu7nto7kdz65umwiyydcb4pa'
      ],
      [
        Buffer.from('a\r\nb'),
        'QmPz7QyeJxVKHADBDuZfW8Ht7u5kEoSjyxmSPGL9UMVyqb',
        'bafkreiayorptnic6fedsociefvqgftsu6gyi743me65ibq47qh5qcdem4i'
      ]
    ]
    for (const [bytes, v0, v1] of encoded) {
      assert.equal(await hashV0(bytes), v0)
      assert.equal(await hashV1(bytes), v1)
      assert.equal(await hashV1(new Uint8Array(bytes)), v1)
    }
  })

  it('hash a Uint8Array of any origin and reject every other value', async () => {
    const empty = 'bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku'
    class Bytes extends Uint8Array {}
    assert.equal(await hashV1(Buffer.alloc(0)), empty)
    assert.equal(await hashV1(new Bytes(0)), empty)
    assert.equal(await hashV1(new Uint8Array(new SharedArrayBuffer(0))), empty)
    assert.equal(await hashV1(new Uint8Array(new ArrayBuffer(8), 8)), empty)
    assert.equal(await hashV1(new Blob([]).stream()), empty)

    const others = [
      null,
      undefined,
      '',
      'text',
      0,
      true,
      {},
      [],
      [1, 2, 3],
      [new Uint8Array(1)],
      new ArrayBuffer(4),
      new DataView(new ArrayBuffer(4)),
      new Uint8ClampedArray(4),
      new Int8Array(4),
      new Uint16Array(4),
      new Blob([]),
      Promise.resolve(new Uint8Array(1)),
      (function* () {
        yield new Uint8Array(1)
      })()
    ]
    for (const value of others) {
      await assert.rejects(hashV0(value as never))
      await assert.rejects(hashV1(value as never))
    }
  })

  // The packages on npm give such a stream a hash, and not the same one in each version.
  it('reject a stream part that is not a Uint8Array', async () => {
    for (const part of ['text', [1, 2], new ArrayBuffer(2), new Uint16Array(2), 5, null, undefined]) {
      async function* stream(): AsyncGenerator<Uint8Array> {
        yield new Uint8Array(1)
        yield part as never
      }
      await assert.rejects(hashV0(stream()), TypeError)
      await assert.rejects(hashV1(stream()), TypeError)
    }
  })

  it('ADR-32 sorts paths by UTF-16 code unit and keeps each path as it is', async () => {
    const folders = [
      entry('models/a/b/c.glb', 'h3'),
      entry('models/a.glb', 'h2'),
      entry('index.js', 'h1'),
      entry('models/a/b.glb', 'h2')
    ]
    const result = await calculateMultipleHashesADR32(folders)
    assert.equal(
      textOf(result.data),
      '{"content":[{"key":"index.js","hash":"h1"},{"key":"models/a.glb","hash":"h2"},' +
        '{"key":"models/a/b.glb","hash":"h2"},{"key":"models/a/b/c.glb","hash":"h3"}]}'
    )
    assert.equal(result.hash, 'bafkreihn5mibjrelaa4i3kejb7h2w7gy2abgnyfazmnxtysti6xisoafrm')
    assert.equal(
      (await calculateMultipleHashesADR32LegacyQmHash(folders)).hash,
      'QmeMPDyzN2PCXqTLmvnjv8eBLUEDDbKonwZ3gCr2aQCkga'
    )

    const sameHash = (files: string[]) => files.map((file) => entry(file, 'h'))
    assert.deepEqual(keysOf(prepareADR32Data(sameHash(['a/b.png', 'A/b.png', 'a/B.png', 'A/B.PNG']))), [
      'A/B.PNG',
      'A/b.png',
      'a/B.png',
      'a/b.png'
    ])
    assert.deepEqual(keysOf(prepareADR32Data(sameHash(['10.png', '2.png', '1.png', '01.png']))), [
      '01.png',
      '1.png',
      '10.png',
      '2.png'
    ])

    const separators = sameHash(['a\\b.png', 'a/b.png', '/a/b.png', './a/b.png', '../a.png', 'a/', 'a//b', ''])
    assert.deepEqual(keysOf(prepareADR32Data(separators)), [
      '',
      '../a.png',
      './a/b.png',
      '/a/b.png',
      'a/',
      'a//b',
      'a/b.png',
      'a\\b.png'
    ])
    assert.equal(
      (await calculateMultipleHashesADR32(separators)).hash,
      'bafkreidcxwea4iy33z4kluef7qc6igyk3x3a22o5o4mnrkn6dzsq3arzk4'
    )

    const names = sameHash([
      'e\u0301',
      '\u{1f600}.png',
      '\u65e5\u672c/\u30d5\u30a1\u30a4\u30eb.png',
      'z',
      '\u00e9',
      '\uffff',
      ' '
    ])
    assert.deepEqual(keysOf(prepareADR32Data(names)), [
      ' ',
      'e\u0301',
      'z',
      '\u00e9',
      '\u65e5\u672c/\u30d5\u30a1\u30a4\u30eb.png',
      '\u{1f600}.png',
      '\uffff'
    ])
    assert.equal(
      (await calculateMultipleHashesADR32(names)).hash,
      'bafkreihxxkrehqhntjbpwdy6vvckxzfxfj4emzpl4ecbufyoyhovhpyrpq'
    )
  })

  it('ADR-32 sorts by hash before path and keeps equal entries', () => {
    const mixed = [entry('a', 'h2'), entry('a', 'h1'), entry('a', 'H1'), entry('b', 'bafk'), entry('c', 'Qm')]
    assert.equal(
      textOf(prepareADR32Data(mixed)),
      '{"content":[{"key":"a","hash":"H1"},{"key":"c","hash":"Qm"},{"key":"b","hash":"bafk"},' +
        '{"key":"a","hash":"h1"},{"key":"a","hash":"h2"}]}'
    )
    assert.equal(
      textOf(prepareADR32Data([entry('a', 'h'), entry('a', 'h'), entry('b', 'h')])),
      '{"content":[{"key":"a","hash":"h"},{"key":"a","hash":"h"},{"key":"b","hash":"h"}]}'
    )
    assert.equal(textOf(prepareADR32Data([])), '{"content":[]}')
    assert.equal(
      textOf(prepareADR32Data([{ file: 'a', hash: 'h', size: 3, key: 'x' } as never])),
      '{"content":[{"key":"a","hash":"h"}]}'
    )
  })

  it('ADR-32 writes the metadata as JSON does', async () => {
    const one = [entry('a', 'h')]
    const written = (metadata?: unknown): string => textOf(prepareADR32Data(one, metadata))
    assert.equal(written(), '{"content":[{"key":"a","hash":"h"}]}')
    assert.equal(written(undefined), written())
    assert.equal(written(null), '{"content":[{"key":"a","hash":"h"}],"metadata":null}')
    assert.equal(written(0), '{"content":[{"key":"a","hash":"h"}],"metadata":0}')
    assert.equal(written(''), '{"content":[{"key":"a","hash":"h"}],"metadata":""}')
    assert.equal(written(false), '{"content":[{"key":"a","hash":"h"}],"metadata":false}')
    assert.equal(
      written({ b: 1, a: { d: [1, null, undefined, 'x'], c: undefined }, e: null }),
      '{"content":[{"key":"a","hash":"h"}],"metadata":{"b":1,"a":{"d":[1,null,null,"x"]},"e":null}}'
    )
    assert.equal(
      written({ nan: NaN, inf: Infinity, neg0: -0, big: 1e21, small: 1e-7 }),
      '{"content":[{"key":"a","hash":"h"}],"metadata":{"nan":null,"inf":null,"neg0":0,"big":1e+21,"small":1e-7}}'
    )
    assert.equal(
      written({ at: new Date(0), j: { toJSON: () => 'replaced' } }),
      '{"content":[{"key":"a","hash":"h"}],"metadata":{"at":"1970-01-01T00:00:00.000Z","j":"replaced"}}'
    )
    assert.equal(
      (await calculateMultipleHashesADR32(one, null)).hash,
      'bafkreifdde6wfusb3o5hymj263m3hcr4bdjs4rbxhyjqdflckzhyrhkvya'
    )
    assert.equal(
      (await calculateMultipleHashesADR32LegacyQmHash(one, null)).hash,
      'QmZKKVAjrdg8gq9esYmQmj14EZUkyZJJfTrBQr4Vu1jkvb'
    )
    assert.equal(
      (await calculateMultipleHashesADR32(one, { b: 1, a: { d: [1, null, undefined, 'x'], c: undefined }, e: null }))
        .hash,
      'bafkreicicc6hbxvt42lnhpykwpkrgr7q7chjjtj4g44iclnykllndkvey4'
    )
    assert.equal((await calculateMultipleHashesADR32([])).hash, 'bafkreienk4dm3ycfbffcpzrmu3rekdkzmgq26hxxatdcmgm4wgxmc24qr4')

    const circular: Record<string, unknown> = {}
    circular.self = circular
    assert.throws(() => prepareADR32Data(one, circular), TypeError)
    assert.throws(() => prepareADR32Data(one, { n: 1n }), TypeError)
  })

  it('ADR-32 does not check the entries and rejects content that is not an array', async () => {
    // 1.2.0 on npm returns this; 1.1.3 and 3.0.4 throw.
    const loose = [{ file: null, hash: 'h' }, { file: 'a', hash: null }, {}] as never
    assert.equal(textOf(prepareADR32Data(loose)), '{"content":[{"key":null,"hash":"h"},{"key":"a","hash":null},{}]}')
    assert.equal(
      (await calculateMultipleHashesADR32(loose)).hash,
      'bafkreiej724kplqjlsepk7dmypadr63hypt42yjn5ky2qzthyjinyfn3m4'
    )
    const frozen = Object.freeze([entry('b', 'h'), entry('a', 'h')]) as never
    assert.deepEqual(keysOf(prepareADR32Data(frozen)), ['a', 'b'])

    for (const value of [null, undefined, 'ab', {}, 5]) {
      assert.throws(() => prepareADR32Data(value as never), TypeError)
      await assert.rejects(calculateMultipleHashesADR32(value as never), TypeError)
      await assert.rejects(calculateMultipleHashesADR32LegacyQmHash(value as never), TypeError)
    }
  })

  it('ADR-62 reads only the own fields that the keys name', () => {
    const empty = 'b48d38f93eaa084033fc5970bf96e559c33c4cdc07d889ab00b4d63f9590739d'
    const onlyA = '25e7c2a96531eb50246780c1f25742e489bf55210e26981dc02992bb585feb97'
    assert.equal(
      keccak256Hash({ a: 1, b: 2 }, ['b', 'a']),
      '67d519fb83383b1b489eab161e4e32ae4f808d6beef9db9e6cde8fff2d84ab44'
    )
    assert.equal(keccak256Hash({ a: 1 }, ['x', 'a', 'y']), onlyA)
    // 1.2.0 on npm gives this; 1.1.3 and 3.0.4 also read the inherited `__proto__`.
    assert.equal(keccak256Hash({ a: 1 }, ['toString', 'constructor', '__proto__', 'a']), onlyA)
    assert.equal(
      keccak256Hash({ a: 1, b: 2 }, ['a', 'b', 'a']),
      'b8ffb64722137f4b100665a52e3c943f8066e8ab8ba3b427e6f4b404defd82b0'
    )
    assert.equal(
      keccak256Hash({ a: null, b: undefined, c: 1 }, ['a', 'b', 'c']),
      '9ff9ecff746dd0a1f3f0e10cfb8b98b459f013253d47673855e2ac6c7aa3750c'
    )
    assert.equal(
      keccak256Hash({ 10: 'a', 2: 'b', x: 1 }, ['x', '10', '2']),
      '4d281e26569b808bba4eb285cb171a018e886cf5fa12224a552cd31fc2a4a34a'
    )
    assert.equal(
      keccak256Hash(
        { '\u00e9': '\u65e5\u672c\u{1f600}', n: { z: 1, y: [null, { x: undefined }] } },
        ['\u00e9', 'n']
      ),
      'cf6f6706bc62ee3ccfc675cb48d92a900764c119777a2510be55fa2b8c299821'
    )
    assert.equal(keccak256Hash({ a: 1 }, []), empty)
    assert.equal(keccak256Hash({}, ['a']), empty)
    assert.throws(() => keccak256Hash(null as never, ['a']), TypeError)
    assert.throws(() => keccak256Hash(undefined as never, ['a']), TypeError)
    assert.throws(() => keccak256Hash({ a: 1 }, null as never), TypeError)
    assert.throws(() => keccak256Hash({ a: 1n }, ['a']), TypeError)
  })
})

describe('ADR-32', () => {
  const one = [{ file: 'a.png', hash: 'bafybeibdik2ihfpcdi7aaaguptwcoc5msav7uhn5hu54xlq2pdwkh5arzy' }]
  const two = [...one, { file: 'a/b.png', hash: 'bafybeibdik2ihfpcdi7aaaguptwcoc5msav7uhn5hu54xlq2pdwkh5asd' }]

  it('hashes the content and the metadata', async () => {
    assert.equal(
      (await calculateMultipleHashesADR32(one)).hash,
      'bafkreigwbjbqaaf63q2cnbrqebctyo3a5y6oxos47usvexhvzajkoczspa'
    )
    assert.equal(
      (await calculateMultipleHashesADR32(two)).hash,
      'bafkreih5bj5fxz72bgvhlqq35teesr75wysn2qcjayyi7kehdcyeiosgdi'
    )
    assert.equal(
      (await calculateMultipleHashesADR32(two, { key: 'value' })).hash,
      'bafkreieusocjbdoxg5cdqtysltk353l3mtmzvtyhza6zxvwfjsfjrcm2ze'
    )
    assert.equal(
      (await calculateMultipleHashesADR32LegacyQmHash(two, { key: 'value' })).hash,
      'QmYLdWnSPor5Ycr6MdaaqwsbTLsnvRtw8MPZ5oitMKoztg'
    )
  })

  it('does not depend on the order of the content and leaves the input as it was', async () => {
    const reversed = [...two].reverse()
    assert.equal(
      (await calculateMultipleHashesADR32(reversed)).hash,
      'bafkreih5bj5fxz72bgvhlqq35teesr75wysn2qcjayyi7kehdcyeiosgdi'
    )
    assert.deepEqual(reversed, [two[1], two[0]])
  })
})

describe('ADR-62', () => {
  const wearable = {
    id: 'urn:decentraland:off-chain:base-avatars:aviatorstyle',
    name: 'Aviator Style',
    description: 'aDescription',
    image: 'image.png',
    thumbnail: 'thumbnail.png',
    data: {
      tags: ['male', 'man', 'base-wearable'],
      category: 'eyewear',
      replaces: [],
      hides: [],
      representations: [
        {
          bodyShapes: ['urn:decentraland:off-chain:base-avatars:BaseMale'],
          mainFile: 'M_Eyewear_AviatorStyle.glb',
          overrideReplaces: [],
          overrideHides: [],
          contents: ['M_Eyewear_AviatorStyle.glb']
        }
      ]
    },
    i18n: [{ code: 'en', text: 'Aviator Style' }],
    createdAt: 1646935739,
    updatedAt: 1646935739,
    metrics: { triangles: 0, materials: 0, meshes: 0, bodies: 0, entities: 0, textures: 0 },
    content: {
      'some-file.glb': '3999dc565303be392b94568fe252fd09482c2329e3381b66d730f870cb6c2afa',
      'thumbnail.png': 'b9b9563ea35e1f995e272e9c699326ac61b94cfe46dc4f49b5215c94d3209854'
    },
    merkleProof: {
      hashingKeys: [
        'id',
        'name',
        'description',
        'image',
        'thumbnail',
        'data',
        'i18n',
        'createdAt',
        'updatedAt',
        'metrics',
        'content'
      ]
    }
  }

  it('hashes the fields the keys name', () => {
    assert.equal(
      keccak256Hash(wearable, wearable.merkleProof.hashingKeys),
      '8282d378bafea28952d4bcce9b2bc1567ed2dda20eba629c8030752dd8169c43'
    )
  })

  it('hashes an empty object when no key matches', () => {
    assert.equal(
      keccak256Hash({ someField: 'someValue' }, ['unknownField']),
      'b48d38f93eaa084033fc5970bf96e559c33c4cdc07d889ab00b4d63f9590739d'
    )
  })

  it('depends on the keys and on their order', () => {
    const metadata = { a: 1, b: 2 }
    assert.notEqual(keccak256Hash(metadata, ['a']), keccak256Hash(metadata, ['b']))
    assert.notEqual(keccak256Hash(metadata, ['a', 'b']), keccak256Hash(metadata, ['b', 'a']))
  })
})
