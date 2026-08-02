// SmugMug publishing module: upload the selected version of a photo into a chosen SmugMug
// album via its official OAuth 1.0a API, and record the publication (marker "smugmug").
// See docs/smugmug.md.

import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";
import {
  OAuthSettings,
  PublishPanel,
  type PublishService,
  type SmugMugAlbum,
} from "./publishing";

// ── Backend commands (owned by this module) ───────────────────────────────────
// Per the module contract, a module's own commands go through `ChairPhotoAPI.invoke`
// rather than core's `api.ts`, so the command names travel with the module.

const smugmugBeginAuth = (api: ChairPhotoAPI) => api.invoke<string>("smugmug_begin_auth");

const smugmugCompleteAuth = (api: ChairPhotoAPI, verifier: string) =>
  api.invoke<void>("smugmug_complete_auth", { verifier });

const smugmugConnected = (api: ChairPhotoAPI) => api.invoke<boolean>("smugmug_connected");

/** The authenticated user's albums (upload targets). */
const smugmugListAlbums = (api: ChairPhotoAPI) =>
  api.invoke<SmugMugAlbum[]>("smugmug_list_albums");

/** Create a new SmugMug album; returns it (uri + name) for the picker. */
const smugmugCreateAlbum = (api: ChairPhotoAPI, name: string) =>
  api.invoke<SmugMugAlbum>("smugmug_create_album", { name });

/** Upload the selected version into a SmugMug album; returns the image URL. */
const postToSmugmug = (
  api: ChairPhotoAPI,
  photoId: number,
  versionId: number | null,
  albumUri: string,
  title: string,
  caption: string,
) =>
  api.invoke<string>("post_to_smugmug", {
    photoId,
    versionId: versionId ?? null,
    albumUri,
    title,
    caption,
  });

/** Bind this module's commands to the host API handle given at load time. */
function makeService(api: ChairPhotoAPI): PublishService {
  return {
    id: "smugmug",
    name: "SmugMug",
    signupUrl: "smugmug.com/api/developer/apply",
    beginAuth: () => smugmugBeginAuth(api),
    completeAuth: (verifier) => smugmugCompleteAuth(api, verifier),
    connected: () => smugmugConnected(api),
    publish: (photoId, versionId, title, caption, albumUri) =>
      postToSmugmug(api, photoId, versionId, albumUri, title, caption),
    listAlbums: () => smugmugListAlbums(api),
    createAlbum: (name) => smugmugCreateAlbum(api, name),
  };
}

export const smugmugModule: ChairPhotoModule = {
  id: "smugmug",
  name: "SmugMug",
  version: "0.1.0",
  description: "Publish photos to a SmugMug album via the official API; records which version was posted.",
  backendFeature: "smugmug",
  publicationMarker: "smugmug",
  onLoad(api) {
    const service = makeService(api);
    api.registerSettingsPanel(() => <OAuthSettings api={api} svc={service} />);
    api.registerPublishTarget({
      id: "smugmug",
      label: "SmugMug",
      render: () => <PublishPanel api={api} svc={service} />,
    });
  },
};
