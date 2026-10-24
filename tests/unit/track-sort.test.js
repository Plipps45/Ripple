'use strict';

const test = require('node:test');
const assert = require('node:assert');

const { TRACK_SORT_MODES, sortTracks, backfillAddedAt } = require('../../src/track-sort');

function track(id, addedAt) {
  return addedAt === undefined ? { id } : { id, addedAt };
}

test('manual order keeps the stored order', () => {
  const list = [track('b', 3), track('a', 1), track('c', 2)];
  assert.deepStrictEqual(sortTracks(list, 'manual').map((t) => t.id), ['b', 'a', 'c']);
});

test('date added newest first', () => {
  const list = [track('old', 1), track('new', 3), track('mid', 2)];
  assert.deepStrictEqual(sortTracks(list, 'added-desc').map((t) => t.id), ['new', 'mid', 'old']);
});

test('date added oldest first', () => {
  const list = [track('old', 1), track('new', 3), track('mid', 2)];
  assert.deepStrictEqual(sortTracks(list, 'added-asc').map((t) => t.id), ['old', 'mid', 'new']);
});

test('tracks without a timestamp count as oldest and keep their order', () => {
  const list = [track('plain-b'), track('dated', 5), track('plain-a')];
  assert.deepStrictEqual(sortTracks(list, 'added-desc').map((t) => t.id), ['dated', 'plain-b', 'plain-a']);
});

test('does not mutate the input list', () => {
  const list = [track('a', 1), track('b', 2)];
  sortTracks(list, 'added-desc');
  assert.deepStrictEqual(list.map((t) => t.id), ['a', 'b']);
});

test('backfill dates untimestamped tracks so newest-first keeps their current order', () => {
  const list = [track('first'), track('second'), track('third')];
  const filled = backfillAddedAt(list, true);
  assert.ok(filled.every((t) => typeof t.addedAt === 'number'));
  assert.deepStrictEqual(sortTracks(filled, 'added-desc').map((t) => t.id), ['first', 'second', 'third']);
  assert.deepStrictEqual(sortTracks(filled, 'added-asc').map((t) => t.id), ['third', 'second', 'first']);
});

test('backfill dates playlists so the last track is treated as the newest', () => {
  const list = [track('first'), track('second'), track('third')];
  const filled = backfillAddedAt(list, false);
  assert.deepStrictEqual(sortTracks(filled, 'added-desc').map((t) => t.id), ['third', 'second', 'first']);
});

test('backfill leaves existing timestamps alone', () => {
  const list = [track('old', 1), track('plain'), track('new', 9)];
  const filled = backfillAddedAt(list, true);
  assert.strictEqual(filled[0].addedAt, 1);
  assert.strictEqual(filled[2].addedAt, 9);
  assert.strictEqual(typeof filled[1].addedAt, 'number');
});

test('backfill returns the same list when every track is already dated', () => {
  const list = [track('a', 1), track('b', 2)];
  assert.strictEqual(backfillAddedAt(list, true), list);
});

test('backfill does not mutate the input list', () => {
  const list = [track('a')];
  backfillAddedAt(list, true);
  assert.strictEqual(list[0].addedAt, undefined);
});

function detail(id, title, artist, duration) {
  return { id, title, artist, duration };
}

test('title sort is alphabetical and ignores case', () => {
  const list = [
    detail('1', 'beta', 'x', 10),
    detail('2', 'Alpha', 'x', 10),
    detail('3', 'gamma', 'x', 10)
  ];
  assert.deepStrictEqual(sortTracks(list, 'title-asc').map((t) => t.id), ['2', '1', '3']);
});

test('title sort puts tracks without a title last', () => {
  const list = [detail('1', '', 'x', 10), detail('2', 'alpha', 'x', 10)];
  assert.deepStrictEqual(sortTracks(list, 'title-asc').map((t) => t.id), ['2', '1']);
});

test('artist sort is alphabetical and keeps ties stable', () => {
  const list = [
    detail('1', 'a', 'Zed', 10),
    detail('2', 'b', 'amy', 10),
    detail('3', 'c', 'amy', 10)
  ];
  assert.deepStrictEqual(sortTracks(list, 'artist-asc').map((t) => t.id), ['2', '3', '1']);
});

test('duration sort runs shortest first and puts unknown lengths last', () => {
  const list = [
    detail('1', 'a', 'x', 240),
    detail('2', 'b', 'x', 0),
    detail('3', 'c', 'x', 95)
  ];
  assert.deepStrictEqual(sortTracks(list, 'duration-asc').map((t) => t.id), ['3', '1', '2']);
});

test('every declared sort mode is available to the picker', () => {
  assert.deepStrictEqual(TRACK_SORT_MODES.slice(0, 3), ['manual', 'added-desc', 'added-asc']);
  assert.ok(TRACK_SORT_MODES.includes('title-asc'));
  assert.ok(TRACK_SORT_MODES.includes('artist-asc'));
  assert.ok(TRACK_SORT_MODES.includes('duration-asc'));
});
