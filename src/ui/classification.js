function route() {
  const key = location.hash.slice(1);
  const target = /^(catalog|fork)-\d+$/.test(key)
    ? document.getElementById(key)
    : null;
  const catalog = target?.classList.contains("catalog") ? target : null;
  document.getElementById("forks").hidden = !!catalog;
  document
    .querySelectorAll(".catalog")
    .forEach((node) => (node.hidden = node !== catalog));
  if (target) target.scrollIntoView();
  else window.scrollTo(0, 0);
}
window.addEventListener("hashchange", route);
document.querySelectorAll(".catalog-search").forEach((input) =>
  input.addEventListener("input", () => {
    const query = input.value.toLowerCase();
    input
      .closest(".catalog")
      .querySelectorAll(".catalog-item")
      .forEach(
        (item) =>
          (item.hidden = !item.textContent.toLowerCase().includes(query)),
      );
  }),
);
route();

function updateCounts() {
  const show = document.body.classList.contains("show-excluded");
  const shown = (n) => show || !n.closest(".llm-excluded");
  const forks = [...document.querySelectorAll("main > .fork")].filter(
    shown,
  ).length;
  const groups = [...document.querySelectorAll("main .patch-group")].filter(
    shown,
  ).length;
  document.getElementById("visible-count").textContent =
    `${forks} forks · ${groups} patch groups`;
}
document
  .getElementById("show-excluded")
  ?.addEventListener("change", (event) => {
    document.body.classList.toggle("show-excluded", event.target.checked);
    updateCounts();
  });
updateCounts();
