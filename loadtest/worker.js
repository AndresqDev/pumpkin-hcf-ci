'use strict';

// Un worker = una CPU. Reparte los bots, los saca del spawn con pathfinder y
// los mete en un bucle de PvP contra los demas bots del prefijo.

const mineflayer = require('mineflayer');
const { pathfinder, Movements, goals } = require('mineflayer-pathfinder');
const { GoalXZ } = goals;

const cfg = JSON.parse(process.argv[2]);
const PHASES = Array.isArray(cfg.phases) && cfg.phases.length ? cfg.phases : null;
const bots = [];
const stats = { deaths: 0, attacks: 0, errors: 0, kicks: 0, reached: 0, stuck: 0, reconnects: 0, disconnects: 0 };
const startedAt = Date.now();
let shuttingDown = false;

// Fase actual segun el reloj compartido (cfg.t0). null = sin fases o test terminado.
function currentPhase() {
  if (!PHASES) return null;
  const elapsed = (Date.now() - (cfg.t0 || startedAt)) / 1000;
  let acc = 0;
  for (const p of PHASES) {
    acc += p.secs;
    if (elapsed < acc) return p;
  }
  return null;
}

const log = (...args) => console.log(`[${new Date().toISOString()}] [w${cfg.worker}]`, ...args);

function createBot(index, attempt = 0) {
  const username = `${cfg.prefix}${String(index).padStart(2, '0')}`;
  const bot = mineflayer.createBot({
    host: cfg.host,
    port: cfg.port,
    username,
    version: cfg.version,
    auth: 'offline',
    checkTimeoutInterval: 60000,
    hideErrors: true,
    // El plugin ya no fuerza el respawn (llamar a player.respawn() desde una
    // tarea del plugin reentra el WASM y congela el tick). El bot pide el
    // respawn con client_command actionId=0, como un cliente vanilla.
    respawn: false,
  });
  bot.loadPlugin(pathfinder);

  // Cuenta packets salientes para diagnosticar los kicks del packet_limiter
  // del host (500/s). El top de nombres se imprime al recibir el kick.
  const pktCounts = Object.create(null);
  let pktTotal = 0;
  let stRef = null;
  const rawWrite = bot._client.write.bind(bot._client);
  bot._client.write = (name, params) => {
    pktTotal++;
    pktCounts[name] = (pktCounts[name] || 0) + 1;
    if (stRef) stRef.pktTotal = pktTotal;
    return rawWrite(name, params);
  };

  const st = { mode: 'join', lastAttack: 0, lastJump: 0, lastGoal: 0, failures: 0, timer: null, respawnTimer: null, scatter: false, scatterAt: 0, scatterGoal: false, pktTotal: 0 };
  stRef = st;
  bot._perf = st;

  const recentChat = [];
  bot.on('message', (msg) => {
    try {
      const text = msg.toString();
      if (text) {
        recentChat.push(text);
        if (recentChat.length > 6) recentChat.shift();
      }
    } catch (_) {}
  });

  const stopPath = () => {
    try { bot.pathfinder.stop(); } catch (_) {}
  };

  // Los bots comparten el world spawn exacto: si se quedan quietos, el
  // maxEntityCramming del host los mata a los ~20-30 s. En idle se les da un
  // destino corto y distinto para que se repartan (sin irse al arena).
  const scatterGoal = () => {
    if (shuttingDown || !bot.entity) return;
    const radius = 2 + (index % Math.max(2, cfg.scatterRadius || 12));
    const angle = (index * 2.399963229728653) % (Math.PI * 2);
    const x = Math.round(bot.entity.position.x + Math.cos(angle) * radius);
    const z = Math.round(bot.entity.position.z + Math.sin(angle) * radius);
    try {
      const moves = new Movements(bot);
      moves.canDig = false;
      moves.allow1by1towers = false;
      moves.allowSprinting = false;
      moves.allowParkour = false;
      moves.scafoldingBlocks = [];
      bot.pathfinder.setMovements(moves);
      bot.pathfinder.setGoal(new GoalXZ(x, z));
      st.scatterGoal = true;
    } catch (e) {
      stats.errors++;
      if (stats.errors < 10) log('scatter failed', username, e.message);
    }
  };

  const setArenaGoal = (cx = cfg.arena.x, cz = cfg.arena.z, minR = 4, addR = 20) => {
    if (shuttingDown || !bot.entity) return;
    const angle = Math.random() * Math.PI * 2;
    const radius = minR + Math.random() * addR;
    const x = Math.round(cx + Math.cos(angle) * radius);
    const z = Math.round(cz + Math.sin(angle) * radius);
    try {
      const moves = new Movements(bot);
      moves.canDig = false;
      moves.allow1by1towers = false;
      moves.allowSprinting = true;
      moves.allowParkour = true;
      moves.scafoldingBlocks = [];
      bot.pathfinder.setMovements(moves);
      bot.pathfinder.setGoal(new GoalXZ(x, z));
      st.scatterGoal = false;
      st.mode = 'travel';
      st.lastGoal = Date.now();
      bot.setControlState('sprint', true);
    } catch (e) {
      stats.errors++;
      st.mode = 'fallback';
      if (stats.errors < 10) log('pathfinder setup failed', username, e.message);
    }
  };

  const afterRespawn = () => {
    st.mode = 'join';
    st.lastJump = 0;
    st.lastGoal = 0;
    st.scatter = false;
    st.scatterGoal = false;
    st.scatterAt = Date.now() + 1000 + Math.random() * 3000;
    clearTimeout(st.respawnTimer);
    bot.clearControlStates();
    stopPath();
    // No caminar durante idle: antes el goal se fijaba igual y los bots se
    // iban 300+ bloques con chunks sin cargar (caidas -> deathban).
    const ph = currentPhase();
    if (!ph || ph.mode !== 'idle') setTimeout(() => setArenaGoal(), 1000);
  };

  // Evita caer al vacio mientras el spawn aun no tiene chunks cargados:
  // mantiene la fisica apagada hasta que blockAt() devuelve un bloque.
  const holdPhysicsUntilLoaded = () => {
    try { bot.physicsEnabled = false; } catch (_) {}
    const t0 = Date.now();
    const tick = () => {
      if (shuttingDown || !bot.entity) return;
      let loaded = false;
      try { loaded = Boolean(bot.blockAt(bot.entity.position)); } catch (_) {}
      if (loaded || Date.now() - t0 > 30000) {
        try { bot.physicsEnabled = true; } catch (_) {}
        return;
      }
      setTimeout(tick, 500);
    };
    setTimeout(tick, 500);
  };

  bot.once('spawn', () => {
    log('spawn', username, bot.entity.position.toString());
    holdPhysicsUntilLoaded();
    afterRespawn();
  });
  bot.on('respawn', () => {
    log('respawn', username);
    holdPhysicsUntilLoaded();
    afterRespawn();
  });
  bot.on('death', () => {
    stats.deaths++;
    try {
      const p = bot.entity && bot.entity.position;
      const below = p ? bot.blockAt(p.offset(0, -1, 0)) : null;
      const at = p ? bot.blockAt(p) : null;
      log('died', username, p ? p.toString() : '?',
        'health=' + (bot.health ?? '?'), 'food=' + (bot.food ?? '?'),
        'below=' + (below && below.name), 'at=' + (at && at.name),
        'dim=' + (bot.game && bot.game.dimension),
        'chat=' + JSON.stringify(recentChat.slice(-3)));
    } catch (_) {}
    st.mode = 'dead';
    bot.clearControlStates();
    stopPath();

    // Un solo client_command por muerte. Reintentar mientras el servidor
    // todavia esta procesando el primero crea varios respawns concurrentes
    // para el mismo jugador y acaba en kicks "Wrong teleport id".
    clearTimeout(st.respawnTimer);
    st.respawnTimer = setTimeout(() => {
      if (shuttingDown || bot.isAlive) return;
      try {
        bot.respawn();
        log('respawn_request', username);
      } catch (e) {
        log('respawn_request_failed', username, e.message);
      }
    }, 250);
  });
  bot.on('goal_reached', () => {
    if (st.scatterGoal) {
      st.scatterGoal = false;
      st.mode = 'idle';
      stopPath();
      bot.clearControlStates();
      return;
    }
    st.mode = 'arena';
    stats.reached++;
    bot.setControlState('sprint', false);
    bot.setControlState('forward', false);
  });
  bot.on('path_update', (r) => {
    if (r.status === 'noPath' || r.status === 'timeout') {
      if (st.scatterGoal) {
        // Un fallo al repartirse no debe convertirse en "stuck" ni en un
        // reintento infinito durante idle: el bot se queda donde esta.
        st.scatterGoal = false;
        stopPath();
        return;
      }
      stats.stuck++;
      if (st.mode === 'travel') {
        st.failures++;
        st.mode = st.failures > 1 ? 'fallback' : 'stuck';
      }
    }
  });
  bot.on('error', (e) => {
    stats.errors++;
    if (stats.errors < 15) log('error', username, e.message);
  });
  bot.on('kicked', (reason) => {
    stats.kicks++;
    const top = Object.entries(pktCounts)
      .sort((a, b) => b[1] - a[1])
      .slice(0, 6)
      .map(([k, v]) => `${k}=${v}`)
      .join(',');
    log('kicked', username, JSON.stringify(reason).slice(0, 140), 'pkts=' + pktTotal, 'top=' + top);
  });
  bot.on('end', (reason) => {
    clearInterval(st.timer);
    clearTimeout(st.respawnTimer);
    const pos = bots.indexOf(bot);
    if (pos >= 0) bots.splice(pos, 1);
    if (shuttingDown) return;
    stats.disconnects++;
    const maxReconnects = cfg.maxReconnects || 10;
    if (attempt < maxReconnects) {
      stats.reconnects++;
      log('reconnect', username, `intento ${attempt + 1}/${maxReconnects}`, 'motivo', reason);
      setTimeout(() => { if (!shuttingDown) createBot(index, attempt + 1); }, 2500 + Math.random() * 2500);
    } else {
      log('end', username, reason);
    }
  });

  st.timer = setInterval(() => {
    if (shuttingDown || !bot.entity || bot.health === undefined) return;
    if (bot.health <= 0) return;

    const ph = currentPhase();
    if (PHASES && ph !== st.phase) {
      st.phase = ph;
      stopPath();
      bot.clearControlStates();
      st.lastGoal = 0;
      st.mode = ph && ph.mode === 'fight' ? 'arena' : 'travel';
    }
    if (ph && ph.mode === 'idle') {
      if (!st.scatter && Date.now() >= st.scatterAt) {
        st.scatter = true;
        scatterGoal();
      }
      return;
    }
    const arena = ph && ph.x !== undefined ? { x: ph.x, z: ph.z } : cfg.arena;
    if (ph && ph.mode === 'move') {
      if (Date.now() - st.lastGoal > 8000) setArenaGoal(arena.x, arena.z, 0, 3);
      return;
    }

    const pos = bot.entity.position;
    const radius = Math.hypot(pos.x, pos.z);

    // Si el respawn no emitio evento, recuperamos por salud + posicion.
    if (st.mode === 'dead' && bot.health > 0) afterRespawn();

    // Dentro del spawn/safezone no se pelea: solo se camina hacia la arena.
    if (radius < 70) {
      if (st.mode === 'arena' || st.mode === 'fight') {
        st.mode = 'travel';
        setArenaGoal();
      }
      if (st.mode === 'join') return;
    }

    let best = null;
    let bestDist = Infinity;
    for (const e of Object.values(bot.entities)) {
      if (!e || e === bot.entity) continue;
      if (!e.username || !e.username.startsWith(cfg.prefix)) continue;
      if (e.isValid === false) continue;
      if (e.health !== undefined && e.health <= 0) continue;
      const d = e.position.distanceTo(pos);
      if (d < bestDist) {
        bestDist = d;
        best = e;
      }
    }

    if (radius >= 70 && best && bestDist < 24) {
      if (st.mode !== 'fight') {
        stopPath();
        st.mode = 'fight';
        bot.setControlState('sprint', false);
      }
      bot.lookAt(best.position.offset(0, 1.5, 0), true);
      if (bestDist > 3.0) {
        bot.setControlState('forward', true);
        if (Date.now() - st.lastJump > 900 && bot.entity.velocity.y === 0) {
          bot.setControlState('jump', true);
          st.lastJump = Date.now();
          setTimeout(() => { try { bot.setControlState('jump', false); } catch (_) {} }, 150);
        }
      } else {
        bot.setControlState('forward', false);
        if (Date.now() - st.lastAttack > 480) {
          try {
            bot.attack(best);
            stats.attacks++;
            st.lastAttack = Date.now();
          } catch (_) {}
        }
      }
      return;
    }

    if (st.mode === 'fight') {
      // El objetivo desaparecio o se alejo: volver a la arena.
      st.mode = 'arena';
      bot.setControlState('forward', false);
    }

    if (st.mode === 'stuck' || st.mode === 'arena') {
      if (Date.now() - st.lastGoal > 8000) setArenaGoal(arena.x, arena.z);
    } else if (st.mode === 'fallback') {
      // Ultimo recurso si el pathfinder no encuentra ruta: caminar recto.
      const dx = arena.x - pos.x;
      const dz = arena.z - pos.z;
      if (Math.hypot(dx, dz) < 4) {
        st.mode = 'arena';
        bot.clearControlStates();
        return;
      }
      bot.lookAt(pos.offset(dx, 0, dz), true);
      bot.setControlState('forward', true);
      bot.setControlState('sprint', true);
      if (Date.now() - st.lastJump > 1100) {
        bot.setControlState('jump', true);
        st.lastJump = Date.now();
        setTimeout(() => { try { bot.setControlState('jump', false); } catch (_) {} }, 150);
      }
    }
  }, 250);

  bots.push(bot);
}

// Conectar con una separacion pequena para no saturar ViaProxy de golpe.
const spacing = cfg.joinSpacing || 900;
for (let i = 0; i < cfg.count; i++) {
  setTimeout(() => createBot(cfg.start + i, 0), i * spacing);
}

setInterval(() => {
  const alive = bots.filter((b) => b.entity && b.player && (b.health ?? 0) > 0).length;
  const stuckNames = bots
    .filter((b) => b.entity && ['fallback', 'stuck'].includes(b._perf?.mode) && Math.hypot(b.entity.position.x, b.entity.position.z) < 65)
    .map((b) => b.player?.username)
    .filter(Boolean);
  const positions = bots
    .filter((b) => b.entity)
    .map((b) => [b.player?.username || '?', Math.round(b.entity.position.x), Math.round(b.entity.position.z), Math.round(b.health ?? 0)]);
  process.send?.({
    type: 'stats',
    worker: cfg.worker,
    bots: bots.length,
    alive,
    deaths: stats.deaths,
    attacks: stats.attacks,
    errors: stats.errors,
    kicks: stats.kicks,
    reconnects: stats.reconnects,
    disconnects: stats.disconnects,
    reached: stats.reached,
    stuck: stats.stuck,
    packets: bots.reduce((s, b) => s + (b._perf?.pktTotal || 0), 0),
    stuckNames,
    positions,
    elapsed: Math.round((Date.now() - startedAt) / 1000),
  });
}, 2000);

function stop() {
  if (shuttingDown) return;
  shuttingDown = true;
  for (const b of bots) {
    try { clearInterval(b._perf.timer); } catch (_) {}
    try { b.quit(); } catch (_) {}
  }
  setTimeout(() => process.exit(0), 1500);
}
process.on('message', (m) => { if (m && m.type === 'stop') stop(); });
process.on('SIGTERM', stop);
process.on('SIGINT', stop);
