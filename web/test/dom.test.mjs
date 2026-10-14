// @ts-check
// The element builder skips empty children instead of printing "null" or "false".
import { test } from 'node:test';
import assert from 'node:assert/strict';

/** A minimal DOM: enough for h() and append() to build a tree and read its text back. */
class FakeNode {
  /** @param {string} name @param {string} [text] */
  constructor(name, text = '') {
    this.nodeName = name;
    this.text = text;
    /** @type {FakeNode[]} */
    this.childNodes = [];
    /** @type {Record<string, string>} */
    this.attributes = {};
    /** @type {Record<string, string>} */
    this.dataset = {};
    this.className = '';
  }
  /** @param {FakeNode} c */
  appendChild(c) {
    this.childNodes.push(c);
    return c;
  }
  /** @param {string} k @param {string} v */
  setAttribute(k, v) {
    this.attributes[k] = v;
  }
  addEventListener() {}
  get textContent() {
    return this.nodeName === '#text' ? this.text : this.childNodes.map((c) => c.textContent).join('');
  }
  set textContent(v) {
    this.childNodes = [new FakeNode('#text', String(v))];
  }
}

const fakeDocument = {
  /** @param {string} tag */
  createElement: (tag) => new FakeNode(tag),
  /** @param {string} s */
  createTextNode: (s) => new FakeNode('#text', s),
  /** @param {string} _ns @param {string} tag */
  createElementNS: (_ns, tag) => new FakeNode(tag),
};

const { h, append } = await import('../app/lib/dom.js');

/** Runs `fn` with the fake document installed, so other test files never see it. @param {() => void} fn */
function withDom(fn) {
  Object.assign(globalThis, { document: fakeDocument });
  try {
    fn();
  } finally {
    Reflect.deleteProperty(globalThis, 'document');
  }
}

test('h() skips null, undefined and false children', () => withDom(() => {
  const p = /** @type {unknown} */ (h('p', null, null, false, undefined, 'x'));
  const node = /** @type {FakeNode} */ (p);
  assert.equal(node.textContent, 'x');
  assert.equal(node.childNodes.length, 1);
}));

test('h() and append() flatten nested arrays of children', () => withDom(() => {
  const ul = /** @type {FakeNode} */ (/** @type {unknown} */ (h('ul', null, ['a', null, ['b', false]], 'c')));
  assert.equal(ul.textContent, 'abc');
  const root = new FakeNode('div');
  append(/** @type {Node} */ (/** @type {unknown} */ (root)), [null, 'y', undefined, [false, 'z']]);
  assert.equal(root.textContent, 'yz');
}));
