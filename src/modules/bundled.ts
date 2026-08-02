import type { ChairPhotoModule } from "./registry";
import { aiTaggingModule } from "./plugins/aiTagging";
import { basicEditorModule } from "./plugins/basicEditor";
import { tagGraphModule } from "./plugins/tagGraph";
import { statisticsModule } from "./plugins/statistics";
import { instagramModule } from "./plugins/instagram";
import { flickrModule } from "./plugins/flickr";
import { smugmugModule } from "./plugins/smugmug";
import { collageModule } from "./plugins/collage";
import { slideshowModule } from "./plugins/slideshow";
import { localsendModule } from "./plugins/localsend";
import { snapchatModule } from "./plugins/snapchat";
import { obsidianModule } from "./plugins/obsidian";
import { mapModule } from "./plugins/map";
import { facesModule } from "./plugins/faces";
import { smartTaggingModule } from "./plugins/smartTagging";

// Modules shipped with the app. The host registers these at startup; each stays
// disabled until the user enables it in the Modules panel.
export const BUNDLED_MODULES: ChairPhotoModule[] = [
  aiTaggingModule,
  basicEditorModule,
  tagGraphModule,
  statisticsModule,
  instagramModule,
  flickrModule,
  smugmugModule,
  collageModule,
  slideshowModule,
  localsendModule,
  snapchatModule,
  obsidianModule,
  mapModule,
  facesModule,
  smartTaggingModule,
];
