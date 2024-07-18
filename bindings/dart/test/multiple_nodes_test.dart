import 'dart:io' as io;
import 'package:test/test.dart';
import 'package:ouisync/ouisync.dart';

void main() {
  late io.Directory temp;
  late Session session1;
  late Session session2;
  late Repository repo1;
  late Repository repo2;

  setUp(() async {
    temp = await io.Directory.systemTemp.createTemp();
    session1 = Session.create(
      kind: SessionKind.unique,
      configPath: '${temp.path}/1/config',
    );
    session2 = Session.create(
      kind: SessionKind.unique,
      configPath: '${temp.path}/2/config',
    );

    repo1 = await Repository.create(
      session1,
      store: '${temp.path}/1/repo.db',
      readSecret: null,
      writeSecret: null,
    );
    await repo1.setSyncEnabled(true);
    final token = await repo1.createShareToken(accessMode: AccessMode.write);

    repo2 = await Repository.create(
      session2,
      store: '${temp.path}/2/repo.db',
      shareToken: token,
      readSecret: null,
      writeSecret: null,
    );
    await repo2.setSyncEnabled(true);

    await session1.bindNetwork(quicV4: "127.0.0.1:0");
    await session2.bindNetwork(quicV4: "127.0.0.1:0");
  });

  tearDown(() async {
    await repo2.close();
    await repo1.close();
    await session2.close();
    await session1.close();
    await temp.delete(recursive: true);
  });

  test('notification on sync', () async {
    final addr = (await session1.quicListenerLocalAddressV4)!;
    await session2.addUserProvidedPeer('quic/$addr');

    // One event for each block created (one for the root directory and one for the file)
    final expect = expectLater(repo2.events, emitsInOrder([null, null]));

    final file = await File.create(repo1, "file.txt");
    await file.close();

    await expect;
  });

  test('notification on peers change', () async {
    final addr = (await session1.quicListenerLocalAddressV4)!;

    final expect = expectLater(
      session2.onPeersChange,
      emitsThrough(contains(isA<PeerInfo>()
          .having((peer) => peer.addr, 'addr', equals('quic/$addr'))
          .having(
              (peer) => peer.source, 'source', equals(PeerSource.userProvided))
          .having((peer) => peer.state, 'state', equals(PeerStateKind.active))
          .having((peer) => peer.runtimeId, 'runtimeId', isNotNull))),
    );

    await session2.addUserProvidedPeer('quic/$addr');

    await expect;
  });

  test('traffic stats', () async {
    final addr = (await session1.quicListenerLocalAddressV4)!;
    await session2.addUserProvidedPeer('quic/$addr');

    final file = await File.create(repo1, 'file.txt');
    await file.close();

    // Wait for the file to get synced
    while (true) {
      try {
        final file = await File.open(repo2, 'file.txt');
        await file.close();
        break;
      } catch (_) {}

      await repo2.events.first;
    }

    final stats = await session2.trafficStats;
    expect(stats.send, greaterThan(0));
    expect(stats.recv, greaterThan(65536)); // at least two blocks received
  });
}
