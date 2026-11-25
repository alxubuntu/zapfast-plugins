const ADMIN_EMAIL = "oneappcity@gmail.com";
const HOST = "wa365.automatiza365.com";

const headers = {
  "Cache-Control": "no-store",
  "X-Content-Type-Options": "nosniff",
  "Referrer-Policy": "no-referrer",
  "Content-Security-Policy":
    "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
};

function json(data, status = 200) {
  return new Response(JSON.stringify(data), {
    status,
    headers: { ...headers, "Content-Type": "application/json; charset=utf-8" },
  });
}

function validPhone(value) {
  return typeof value === "string" && /^\+[1-9]\d{1,14}$/.test(value);
}

function validDate(value) {
  if (typeof value !== "string" || !/^\d{4}-\d{2}-\d{2}$/.test(value)) return false;
  const date = new Date(`${value}T00:00:00.000Z`);
  return !Number.isNaN(date.valueOf()) && date.toISOString().slice(0, 10) === value;
}

async function routeApi(request, env, url) {
  const origin = request.headers.get("Origin");
  if (origin && origin !== `https://${HOST}`) return json({ error: "Origen no permitido." }, 403);

  if (url.pathname === "/api/licenses" && request.method === "GET") {
    const { results } = await env.LICENSES.prepare(
      "SELECT phone, active, starts_at, expires_at FROM licenses ORDER BY phone",
    ).all();
    return json(results);
  }

  if (url.pathname === "/api/licenses" && request.method === "POST") {
    const body = await request.json().catch(() => null);
    if (
      !body ||
      !validPhone(body.phone) ||
      !validDate(body.starts_at) ||
      !validDate(body.expires_at) ||
      body.expires_at < body.starts_at ||
      ![0, 1].includes(body.active)
    ) {
      return json({ error: "Revisa el teléfono, estado y fechas." }, 400);
    }
    try {
      await env.LICENSES.prepare(
        "INSERT INTO licenses (phone, active, starts_at, expires_at) VALUES (?, ?, ?, ?)",
      )
        .bind(body.phone, body.active, body.starts_at, body.expires_at)
        .run();
      return json({ ok: true }, 201);
    } catch (error) {
      if (String(error).includes("UNIQUE")) {
        return json({ error: "Ese teléfono ya tiene una licencia." }, 409);
      }
      throw error;
    }
  }

  if (url.pathname === "/api/licenses" && request.method === "PATCH") {
    const body = await request.json().catch(() => null);
    if (
      !body ||
      !validPhone(body.phone) ||
      !validDate(body.starts_at) ||
      !validDate(body.expires_at) ||
      body.expires_at < body.starts_at ||
      ![0, 1].includes(body.active)
    ) {
      return json({ error: "Revisa el estado y las fechas." }, 400);
    }
    const result = await env.LICENSES.prepare(
      "UPDATE licenses SET active = ?, starts_at = ?, expires_at = ? WHERE phone = ?",
    )
      .bind(body.active, body.starts_at, body.expires_at, body.phone)
      .run();
    if (result.meta.changes === 0) return json({ error: "Licencia no encontrada." }, 404);
    return json({ ok: true });
  }

  return json({ error: "Ruta no encontrada." }, 404);
}

async function checkLicense(request, env) {
  if (request.method !== "POST") {
    return json({ error: "Método no permitido." }, 405);
  }
  const body = await request.json().catch(() => null);
  if (!body || !validPhone(body.phone)) {
    return json({ error: "Número de teléfono no válido." }, 400);
  }
  const row = await env.LICENSES.prepare(
    "SELECT 1 AS valid FROM licenses WHERE phone = ? AND active = 1 AND date('now') BETWEEN starts_at AND expires_at LIMIT 1",
  )
    .bind(body.phone)
    .first();
  return json({ valid: Boolean(row) });
}

const page = `<!doctype html>
<html lang="es">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>WA365 · Licencias</title>
<style>
:root{font:15px system-ui,sans-serif;color:#17202a;background:#f4f6f8}*{box-sizing:border-box}
body{margin:0}.wrap{max-width:1000px;margin:40px auto;padding:0 20px}
header{display:flex;align-items:center;justify-content:space-between;gap:16px;margin-bottom:22px}
h1{font-size:24px;margin:0}button{border:0;border-radius:7px;padding:10px 14px;background:#1463df;color:white;font:inherit;cursor:pointer}
button.secondary{background:#e8edf4;color:#17202a}button:disabled{opacity:.6;cursor:wait}
.card{background:#fff;border:1px solid #e2e7ed;border-radius:10px;overflow:hidden}
table{width:100%;border-collapse:collapse}th,td{text-align:left;padding:12px 14px;border-bottom:1px solid #edf0f3}
th{font-size:12px;text-transform:uppercase;color:#667085;background:#fafbfc}tr:last-child td{border-bottom:0}
.status{font-weight:600}.active{color:#12805c}.inactive{color:#a13c38}
.empty{padding:32px;text-align:center;color:#667085}.message{min-height:24px;margin:10px 0;color:#455468}
dialog{border:0;border-radius:12px;width:min(440px,calc(100% - 28px));padding:22px;box-shadow:0 16px 60px #0003}
dialog::backdrop{background:#10182888}form{display:grid;gap:14px}label{display:grid;gap:6px;font-size:14px}
input,select{width:100%;padding:10px;border:1px solid #cfd6df;border-radius:6px;font:inherit}
.actions{display:flex;justify-content:flex-end;gap:8px;margin-top:4px}.actions button{min-width:90px}
@media(max-width:650px){.wrap{margin:22px auto;padding:0 12px;overflow-x:auto}table{min-width:650px}header{align-items:flex-start}}
</style>
</head>
<body><main class="wrap">
<header><h1>Licencias WA365</h1><button id="add">Agregar licencia</button></header>
<div id="message" class="message" role="status"></div>
<section class="card"><table><thead><tr><th>Teléfono</th><th>Estado</th><th>Inicio</th><th>Caducidad</th><th></th></tr></thead><tbody id="rows"></tbody></table><div id="empty" class="empty" hidden>No hay licencias todavía.</div></section>
</main>
<dialog id="editor"><form id="form">
<h2 id="title" style="margin:0">Agregar licencia</h2>
<label>Teléfono (formato internacional)<input id="phone" placeholder="+51933086539" required pattern="\\+[1-9][0-9]{1,14}" maxlength="16"></label>
<label>Fecha de inicio<input id="starts" type="date" required></label>
<label>Fecha de caducidad<input id="expires" type="date" required></label>
<label>Estado<select id="active"><option value="1">Activa</option><option value="0">Suspendida</option></select></label>
<div class="actions"><button type="button" class="secondary" id="cancel">Cancelar</button><button id="save">Guardar</button></div>
</form></dialog>
<script>
const rows=document.querySelector("#rows"),empty=document.querySelector("#empty"),message=document.querySelector("#message"),dialog=document.querySelector("#editor"),form=document.querySelector("#form");
let editing=null;
function esc(s){return String(s).replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]))}
async function load(){message.textContent="";try{const r=await fetch("/api/licenses");if(!r.ok)throw Error("No se pudo cargar la lista.");const list=await r.json();rows.innerHTML=list.map(x=>'<tr><td>'+esc(x.phone)+'</td><td class="status '+(x.active?"active":"inactive")+'">'+(x.active?"Activa":"Suspendida")+'</td><td>'+esc(x.starts_at)+'</td><td>'+esc(x.expires_at)+'</td><td><button class="secondary edit" data-phone="'+esc(x.phone)+'">Editar</button></td></tr>').join("");empty.hidden=list.length!==0;rows.querySelectorAll(".edit").forEach(b=>b.onclick=()=>edit(list.find(x=>x.phone===b.dataset.phone)))}catch(e){message.textContent=e.message}}
function edit(x){editing=x.phone;document.querySelector("#title").textContent="Editar licencia";document.querySelector("#phone").value=x.phone;document.querySelector("#phone").disabled=true;document.querySelector("#starts").value=x.starts_at;document.querySelector("#expires").value=x.expires_at;document.querySelector("#active").value=String(x.active);dialog.showModal()}
document.querySelector("#add").onclick=()=>{editing=null;form.reset();document.querySelector("#title").textContent="Agregar licencia";document.querySelector("#phone").disabled=false;dialog.showModal()};
document.querySelector("#cancel").onclick=()=>dialog.close();
form.onsubmit=async e=>{e.preventDefault();const save=document.querySelector("#save");save.disabled=true;try{const body={phone:document.querySelector("#phone").value,starts_at:document.querySelector("#starts").value,expires_at:document.querySelector("#expires").value,active:Number(document.querySelector("#active").value)};const r=await fetch("/api/licenses",{method:editing?"PATCH":"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(body)});const data=await r.json();if(!r.ok)throw Error(data.error||"No se pudo guardar.");dialog.close();await load();message.textContent="Cambios guardados."}catch(err){message.textContent=err.message}finally{save.disabled=false}};
load();
</script></body></html>`;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.hostname !== HOST) return new Response("Not found", { status: 404, headers });
    if (url.pathname === "/api/check-license") {
      try {
        return await checkLicense(request, env);
      } catch {
        return json({ error: "No se pudo consultar la licencia." }, 503);
      }
    }
    if (url.pathname.startsWith("/api/")) {
      if (request.headers.get("Cf-Access-Authenticated-User-Email") !== ADMIN_EMAIL) {
        return json({ error: "No autorizado." }, 401);
      }
      try {
        return await routeApi(request, env, url);
      } catch {
        return json({ error: "Error al consultar la base de datos." }, 503);
      }
    }
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("Method not allowed", { status: 405, headers });
    }
    if (request.headers.get("Cf-Access-Authenticated-User-Email") !== ADMIN_EMAIL) {
      return new Response("No autorizado.", { status: 401, headers });
    }
    return new Response(request.method === "HEAD" ? null : page, {
      headers: { ...headers, "Content-Type": "text/html; charset=utf-8" },
    });
  },
};
